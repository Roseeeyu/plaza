//! The server: shared canvas state, sessions, and live diff fan-out.
//!
//! # Concurrency
//!
//! - Every connection is a pair of tokio tasks: one reads and processes
//!   requests, the other writes replies from the session's [`OutQueue`].
//!   Tasks are cheap, so thousands of idle connections cost almost nothing,
//!   and a client that stops reading only ever stalls its own writer task.
//! - All canvas state lives in one `std::sync::Mutex<Shared>`. Critical
//!   sections are short and never `.await`, so a blocking mutex is both
//!   correct and faster than an async one here. Every request takes it at
//!   most once, so no request can observe a half-applied stamp.
//! - Diffs are pushed into subscribers' queues while the canvas lock is
//!   held, so they are queued in seq order. The only lock taken inside the
//!   canvas lock is a queue's own, and nothing takes them the other way
//!   round, so the two cannot deadlock.
//! - A subscriber whose queue would hold more than `lag_budget` diffs gets
//!   `lagged` and loses its subscription, which bounds memory per client.
//! - Disk I/O runs on a dedicated persister thread (see [`crate::persist`]),
//!   never on the async workers.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

use crate::canvas::{Canvas, Commit};
use crate::color::Color;
use crate::geometry::Rect;
use crate::persist;
use crate::protocol::{Reply, Request};
use crate::{tcp, web};

/// The largest stamp, in pixels along each side.
pub const MAX_STAMP_DIM: u32 = 8;

/// Longest request line accepted, in bytes.
pub const MAX_LINE: usize = 4096;

/// A client that does not accept a reply within this long is disconnected.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub cooldown_ms: u64,
    pub lag_budget: usize,
    pub data_dir: Option<PathBuf>,
    pub snapshot_every: u64,
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    cooldown_ms: u64,
    lag_budget: usize,
}

/// Milliseconds since the server started.
fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// A panic in one session must not take the whole canvas down with it, and
/// every critical section leaves the state consistent, so poisoning can be
/// safely ignored.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything sessions share, handed to every connection.
pub struct State {
    shared: Mutex<Shared>,
    limits: Limits,
    next_id: AtomicU64,
}

/// Runs the server until `shutdown` resolves. `web` serves the frontend and
/// WebSocket sessions; `tcp`, if given, serves the raw line protocol.
pub async fn serve(
    web: TcpListener,
    tcp: Option<TcpListener>,
    config: Config,
    shutdown: impl Future<Output = ()>,
) -> io::Result<()> {
    let (canvas, log, persister) = match &config.data_dir {
        Some(dir) => {
            let recovered = persist::recover(dir, config.width, config.height)?;
            eprintln!(
                "recovered {} at seq {} ({} log records replayed, {} damaged bytes discarded)",
                dir.display(),
                recovered.canvas.seq(),
                recovered.replayed,
                recovered.discarded_bytes,
            );
            let (log, persister) =
                persist::spawn(dir.clone(), recovered.canvas.clone(), config.snapshot_every)?;
            (recovered.canvas, Some(log), Some(persister))
        }
        None => (Canvas::new(config.width, config.height), None, None),
    };

    let state = Arc::new(State {
        shared: Mutex::new(Shared {
            canvas,
            subs: HashMap::new(),
            next_write_ms: HashMap::new(),
            log,
        }),
        limits: Limits {
            cooldown_ms: config.cooldown_ms,
            lag_budget: config.lag_budget,
        },
        next_id: AtomicU64::new(1),
    });

    let web = web::serve(web, Arc::clone(&state));
    let tcp = async {
        match tcp {
            Some(listener) => tcp::serve(listener, Arc::clone(&state)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        result = web => result?,
        result = tcp => result?,
        () = shutdown => eprintln!("shutting down"),
    }

    finish_persisting(&state, persister).await;
    Ok(())
}

/// Closes the persister's channel and waits for it to write its final
/// snapshot, so a clean shutdown leaves an empty log behind.
async fn finish_persisting(state: &State, persister: Option<JoinHandle<()>>) {
    lock(&state.shared).log = None;
    if let Some(handle) = persister {
        let _ = tokio::task::spawn_blocking(move || handle.join()).await;
    }
}

struct Subscription {
    region: Rect,
    queue: Arc<OutQueue>,
}

struct Shared {
    canvas: Canvas,
    /// Live subscriptions, keyed by session id.
    subs: HashMap<u64, Subscription>,
    /// When each nickname may next write. Keyed by name rather than by
    /// connection, so opening more tabs does not dodge the cooldown.
    next_write_ms: HashMap<Arc<str>, u64>,
    /// Commits are sent here to be made durable, if persistence is on.
    log: Option<Sender<Commit>>,
}

impl Shared {
    fn commit(
        &mut self,
        author: &Arc<str>,
        region: Rect,
        colors: Vec<Color>,
        limits: Limits,
    ) -> Result<u64, String> {
        let commit = self.canvas.commit(author, region, colors)?;
        let seq = commit.seq;

        self.subs.retain(|_, sub| {
            let diffs: Vec<Reply> = commit
                .cells()
                .filter(|&(pos, _)| sub.region.contains(pos))
                .map(|(pos, color)| Reply::Diff {
                    seq,
                    pos,
                    color,
                    author: Arc::clone(author),
                })
                .collect();
            diffs.is_empty() || sub.queue.offer_diffs(diffs, limits.lag_budget)
        });

        // An unbounded std channel never blocks, so this is safe to do
        // while holding the lock.
        if let Some(log) = &self.log
            && log.send(commit).is_err()
        {
            eprintln!("persister has stopped; continuing without persistence");
            self.log = None;
        }
        if limits.cooldown_ms > 0 {
            let wait = limits.cooldown_ms.saturating_mul(region.area());
            self.next_write_ms
                .insert(Arc::clone(author), now_ms().saturating_add(wait));
        }
        Ok(seq)
    }
}

/// Runs one client session over any transport: `incoming` yields request
/// lines, `outgoing` accepts reply text. Returns when the client goes away.
pub async fn run_session<I, O>(incoming: I, outgoing: O, state: Arc<State>)
where
    I: Stream<Item = String>,
    O: Sink<String> + Send + 'static,
{
    let mut session = Session {
        id: state.next_id.fetch_add(1, Ordering::Relaxed),
        name: None,
        queue: OutQueue::new(),
    };

    let queue = Arc::clone(&session.queue);
    let mut writer = tokio::spawn(async move {
        let mut outgoing = pin!(outgoing);
        while let Some(reply) = queue.pop().await {
            let sent = tokio::time::timeout(WRITE_TIMEOUT, outgoing.send(reply.to_string())).await;
            if !matches!(sent, Ok(Ok(()))) {
                break;
            }
        }
        // Stop accepting diffs for a client that is gone.
        queue.close();
    });

    let mut incoming = pin!(incoming);
    let mut writer_done = false;
    loop {
        tokio::select! {
            line = incoming.next() => {
                let Some(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let reply = match line.parse::<Request>() {
                    Ok(request) => session.process(request, &state),
                    Err(message) => Some(Reply::Error(message)),
                };
                if let Some(reply) = reply {
                    session.queue.push(reply);
                }
            }
            // If the client stops accepting replies, drop it even if it is
            // still sending.
            _ = &mut writer => {
                writer_done = true;
                break;
            }
        }
    }

    lock(&state.shared).subs.remove(&session.id);
    session.queue.close();
    if !writer_done {
        let _ = writer.await;
    }
}

/// One connection's state, owned by its reader task.
struct Session {
    id: u64,
    name: Option<Arc<str>>,
    queue: Arc<OutQueue>,
}

impl Session {
    /// Handles one request. Returns the reply to queue, or `None` if the
    /// reply was already queued.
    fn process(&mut self, request: Request, state: &State) -> Option<Reply> {
        let reply = match request {
            Request::Hello { name } => {
                let name: Arc<str> = name.into();
                self.name = Some(Arc::clone(&name));
                Reply::Welcome { name }
            }
            Request::Paint { pos, color } => {
                let region = Rect {
                    col: pos.col,
                    row: pos.row,
                    width: 1,
                    height: 1,
                };
                self.write(state, region, vec![color])
                    .map_or_else(|r| r, |seq| Reply::Painted { seq })
            }
            Request::Stamp { region, colors } => {
                if region.width > MAX_STAMP_DIM || region.height > MAX_STAMP_DIM {
                    return Some(Reply::Error(format!(
                        "stamps may be at most {MAX_STAMP_DIM}x{MAX_STAMP_DIM}"
                    )));
                }
                self.write(state, region, colors)
                    .map_or_else(|r| r, |seq| Reply::Stamped { seq })
            }
            Request::Read { pos } => match lock(&state.shared).canvas.read(pos) {
                Ok((color, author)) => Reply::Pixel { pos, color, author },
                Err(message) => Reply::Error(message),
            },
            Request::Snapshot { region } => {
                let shared = lock(&state.shared);
                match shared.canvas.snapshot(region) {
                    Ok((region, rows)) => Reply::Snapshot {
                        seq: shared.canvas.seq(),
                        region,
                        rows,
                    },
                    Err(message) => Reply::Error(message),
                }
            }
            Request::Subscribe { region } => {
                let mut shared = lock(&state.shared);
                if !shared.canvas.board().contains_rect(&region) {
                    return Some(Reply::Error(format!("region ({region}) is off the canvas")));
                }
                let seq = shared.canvas.seq();
                shared.subs.insert(
                    self.id,
                    Subscription {
                        region,
                        queue: Arc::clone(&self.queue),
                    },
                );
                // Queue the reply before releasing the lock, so it is
                // guaranteed to arrive before any diff after `seq`.
                self.queue.push(Reply::Subscribed { seq });
                return None;
            }
            Request::Unsubscribe => {
                lock(&state.shared).subs.remove(&self.id);
                Reply::Unsubscribed
            }
        };
        Some(reply)
    }

    /// Commits a paint or stamp. The cooldown check and the commit happen
    /// under one lock hold, so two tabs sharing a nickname cannot both slip
    /// through the same cooldown window.
    fn write(&self, state: &State, region: Rect, colors: Vec<Color>) -> Result<u64, Reply> {
        let Some(name) = &self.name else {
            return Err(Reply::Error(
                "say 'hello <name>' before painting".to_string(),
            ));
        };
        let mut shared = lock(&state.shared);
        let now = now_ms();
        if let Some(&next) = shared.next_write_ms.get(name)
            && now < next
        {
            return Err(Reply::Cooldown {
                remaining_ms: next - now,
            });
        }
        shared
            .commit(name, region, colors, state.limits)
            .map_err(Reply::Error)
    }
}

/// Replies waiting to be written to one client.
///
/// A plain channel will not do: when a subscriber lags, the diffs already
/// queued for it must be taken back out and replaced by `lagged`.
struct OutQueue {
    inner: Mutex<OutInner>,
    /// Wakes the writer task. There is only ever one waiter, and
    /// `notify_one` stores a permit when nobody is waiting yet, so a wakeup
    /// that races ahead of the writer is never lost.
    ready: Notify,
}

struct OutInner {
    items: VecDeque<Reply>,
    /// How many of `items` are diffs, checked against the lag budget.
    diffs: usize,
    closed: bool,
}

impl OutQueue {
    fn new() -> Arc<OutQueue> {
        Arc::new(OutQueue {
            inner: Mutex::new(OutInner {
                items: VecDeque::new(),
                diffs: 0,
                closed: false,
            }),
            ready: Notify::new(),
        })
    }

    fn push(&self, reply: Reply) {
        let mut inner = lock(&self.inner);
        if !inner.closed {
            inner.items.push_back(reply);
            self.ready.notify_one();
        }
    }

    /// Queues every diff of one commit, or none of them: if they would take
    /// the client over budget, its pending diffs are replaced by `lagged`
    /// and `false` is returned so the caller drops the subscription.
    fn offer_diffs(&self, diffs: Vec<Reply>, budget: usize) -> bool {
        let mut inner = lock(&self.inner);
        if inner.closed {
            return false;
        }
        let fits = inner.diffs + diffs.len() <= budget;
        if fits {
            inner.diffs += diffs.len();
            inner.items.extend(diffs);
        } else {
            inner.items.retain(|r| !matches!(r, Reply::Diff { .. }));
            inner.diffs = 0;
            inner.items.push_back(Reply::Lagged);
        }
        self.ready.notify_one();
        fits
    }

    fn try_pop(&self) -> Option<Option<Reply>> {
        let mut inner = lock(&self.inner);
        if let Some(item) = inner.items.pop_front() {
            if matches!(item, Reply::Diff { .. }) {
                inner.diffs -= 1;
            }
            return Some(Some(item));
        }
        inner.closed.then_some(None)
    }

    /// Waits for a reply, or returns `None` once the queue is closed and
    /// drained. Cancel-safe: an item is only removed right before returning.
    async fn pop(&self) -> Option<Reply> {
        loop {
            if let Some(item) = self.try_pop() {
                return item;
            }
            self.ready.notified().await;
        }
    }

    fn close(&self) {
        lock(&self.inner).closed = true;
        self.ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Pos;

    fn diff(seq: u64) -> Reply {
        Reply::Diff {
            seq,
            pos: Pos { col: 0, row: 0 },
            color: Color::WHITE,
            author: "ann".into(),
        }
    }

    #[tokio::test]
    async fn over_budget_replaces_pending_diffs_with_lagged() {
        let queue = OutQueue::new();
        queue.push(Reply::Painted { seq: 1 });
        assert!(queue.offer_diffs(vec![diff(1), diff(1)], 3));
        assert!(!queue.offer_diffs(vec![diff(2), diff(2)], 3));
        queue.close();
        assert_eq!(queue.pop().await, Some(Reply::Painted { seq: 1 }));
        assert_eq!(queue.pop().await, Some(Reply::Lagged));
        assert_eq!(queue.pop().await, None);
    }

    #[tokio::test]
    async fn pop_wakes_when_a_reply_arrives() {
        let queue = OutQueue::new();
        let writer = {
            let queue = Arc::clone(&queue);
            tokio::spawn(async move { queue.pop().await })
        };
        tokio::task::yield_now().await;
        queue.push(Reply::Unsubscribed);
        assert_eq!(writer.await.unwrap(), Some(Reply::Unsubscribed));
    }
}
