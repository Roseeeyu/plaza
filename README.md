# Plaza

A real-time multiplayer pixel canvas in the spirit of r/place, written in Rust.
Everyone paints on one shared board in the browser, every stroke streams live
to everyone else, and the canvas survives crashes and restarts.

- **Live updates** over WebSocket to the browser, or over a plain-text line
  protocol you can drive with `nc`.
- **Atomic stamps**: multi-pixel writes that no reader can ever see half-applied.
- **Per-user rate limiting** that follows the nickname, not the connection.
- **Crash-safe persistence**: a checksummed write-ahead log plus atomic
  snapshots, with automatic recovery from torn writes.
- **Backpressure**: a slow client is cut off and resynced instead of holding
  memory or slowing anyone else down.

## Quick start

```bash
cargo run --release
# open http://127.0.0.1:8080, pick a nickname, and paint
```

Open a second tab to watch strokes arrive live. To let other devices on your
network join, listen on all interfaces and share your machine's IP:

```bash
cargo run --release -- 0.0.0.0:8080
```

### With Docker

```bash
docker build -t plaza .
docker run -p 8080:8080 -p 8081:8081 -v plaza-data:/data plaza
```

The image runs as a non-root user, stores the canvas in the `/data` volume,
shuts down cleanly on `docker stop`, and reports health via `GET /healthz`.

### Options

| Flag | Default | Meaning |
| --- | --- | --- |
| `[ADDR]` | `127.0.0.1:8080` | Web frontend and WebSocket address |
| `--tcp <ADDR>` | off | Also serve the raw line protocol here |
| `--width`, `--height` | `64` | Canvas size for a new canvas |
| `--cooldown <MS>` | `0` | Wait per pixel written before the same user may write again |
| `--lag-budget <N>` | `1024` | Diffs that may queue for one subscriber before it is dropped |
| `--data <DIR>` | `data` | Where the canvas is saved |
| `--no-persist` | | Keep the canvas in memory only |
| `--snapshot-every <N>` | `1000` | Commits between snapshots |

## Protocol

One request per line. Colours are names (`red`) or hex codes (`5`) from a
16-colour palette. Reading is open to everyone; writing needs a nickname.

```text
hello <name>                       -> welcome <name>
paint <col> <row> <color>          -> painted <seq>      | cooldown <ms>
stamp <col> <row> <w> <h> <codes>  -> stamped <seq>      | cooldown <ms>
read <col> <row>                   -> pixel <col> <row> <color> <author|->
snapshot [<col> <row> <w> <h>]     -> snapshot <seq> <col> <row> <w> <h>
                                      followed by one line of colour codes per row
subscribe <col> <row> <w> <h>      -> subscribed <seq>
                                      then  diff <seq> <col> <row> <color> <author>
                                      or    lagged
unsubscribe                        -> unsubscribed
```

Every write gets the next sequence number. All diffs from one stamp share its
`seq` and arrive back to back. A client syncs by subscribing first and then
taking a snapshot: every diff with a `seq` above the snapshot's is new.

```console
$ cargo run --release -- --tcp 127.0.0.1:8081 &
$ nc 127.0.0.1 8081
hello ann
welcome ann
stamp 0 0 2 2 5c5c
stamped 1
read 1 0
pixel 1 0 blue ann
```

## Architecture

```text
 browser ──WebSocket /ws──┐                     ┌──────────────────────────┐
                          ├─► session (2 tasks) ─┤ Mutex<Shared>            │
 nc ──────TCP lines───────┘    reader ─ process  │  canvas · subscriptions  │
                               writer ◄─ OutQueue│  cooldowns · log sender ─┼──► persister thread
                                    ▲            └───────────┬──────────────┘     canvas.log (WAL)
                                    └──── diffs fanned out ──┘                    canvas.snap
```

| Module | Responsibility |
| --- | --- |
| `protocol` | Parses request lines, formats replies |
| `canvas` | Pixels, per-pixel authorship, the commit counter |
| `server` | Sessions, cooldowns, subscriptions, diff fan-out |
| `persist` | Write-ahead log, snapshots, crash recovery |
| `web`, `tcp` | Adapt WebSocket (axum) and raw TCP into one session interface |

### Concurrency

- **Two tokio tasks per connection.** One reads and handles requests; the other
  drains that client's outgoing queue. A client that stops reading only stalls
  its own writer task.
- **One short-held lock for all canvas state.** Each request takes it at most
  once and never awaits while holding it, so a stamp is atomic, there is no
  lock ordering to get wrong, and a blocking `std::sync::Mutex` beats an async
  one.
- **Ordered fan-out.** Diffs are queued while the lock is held, so every
  subscriber receives them in `seq` order. The only lock nested inside it is a
  queue's own, never taken the other way round, so they cannot deadlock.
- **Bounded memory per client.** If a commit would push a subscriber past its
  lag budget, its pending diffs are replaced by one `lagged` message and the
  subscription is dropped. The frontend then resubscribes and resyncs.

### Persistence

- **Write-ahead log.** Every commit is appended to `canvas.log` as
  `[length][CRC32][payload]`.
- **Snapshots.** Every N commits the full canvas is written to a temporary
  file, fsynced, and renamed over `canvas.snap`. The rename is atomic, so a
  crash leaves either the old snapshot or the new one, never a mix. The log is
  truncated afterwards.
- **Recovery.** On startup Plaza loads the snapshot and replays the log after
  it. A record cut short by a crash fails its length or checksum check and is
  truncated away. Replay skips commits the snapshot already contains, so it is
  safe to repeat.
- **Off the request path.** Disk I/O runs on its own thread. The server hands
  it each commit over a channel and never waits for the disk. The persister
  keeps a replica of the canvas, so snapshots do not need the server's lock,
  and it fsyncs once per batch of queued commits (group commit).

The trade-off: a write is acknowledged slightly before it is fsynced, so a
power loss can drop the last few milliseconds of writes. A process crash loses
nothing that reached the log.

## Performance

Plaza began as a thread-per-connection server (a reader and a writer thread
per client) and was then ported to tokio. Same machine (Apple M1), same
benchmark, release builds, persistence off, median of three runs:

| 1,000 concurrent subscribers | Threads | Async (tokio) |
| --- | --- | --- |
| OS threads | 2,001 | 9 |
| Resident memory | 42 MB | 20 MB |
| One 8×8 stamp reaching every subscriber (64,000 diffs) | 50 ms | 49 ms |

Fan-out latency is unchanged: the work is the same, only who runs it changed.
The win is in what each idle connection costs, which is what limits how many
clients one machine can hold.

To reproduce:

```bash
cargo run --release -- --tcp 127.0.0.1:8081 --no-persist --lag-budget 100000 &
ulimit -n 4096
python3 bench/fanout.py $! --port 8081 --clients 1000
```

## Development

```bash
cargo test                  # 20 unit tests + 6 end-to-end tests
cargo clippy --all-targets  # zero warnings
cargo fmt --check
```

The end-to-end tests start real servers on random ports. They cover
authorship, live diffs, cooldowns shared across tabs, lagging subscribers,
restart survival, and the HTTP endpoints. The persistence tests also corrupt
the log on purpose to check that recovery truncates it.
