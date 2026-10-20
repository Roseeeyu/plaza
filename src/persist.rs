//! Durable storage: a write-ahead log of commits plus periodic snapshots.
//!
//! - `canvas.log` is append-only. Each record is framed as
//!   `[len: u32][crc32: u32][payload]`, so a record half-written by a crash
//!   is detected on startup and cut off.
//! - `canvas.snap` is a full copy of the canvas at some seq, written to a
//!   temporary file, fsynced, then atomically renamed into place.
//!
//! Recovery loads the snapshot (if any) and replays every log record after
//! it. Replay skips commits the snapshot already contains, so a crash
//! between writing a snapshot and truncating the log is harmless.
//!
//! All disk I/O happens on one persister thread. The server only sends it
//! commits over a channel while holding the canvas lock (which keeps the
//! log in seq order) and never waits on the disk itself. The persister
//! keeps its own replica of the canvas, so taking a snapshot never needs
//! the server's lock either.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use crate::canvas::Canvas;
use crate::canvas::Commit;
use crate::color::Color;
use crate::geometry::Rect;

const LOG_FILE: &str = "canvas.log";
const SNAP_FILE: &str = "canvas.snap";
const SNAP_TMP_FILE: &str = "canvas.snap.tmp";
const SNAP_MAGIC: &[u8; 8] = b"PLAZASN1";
/// Upper bound on one record's payload, to reject garbage lengths.
const MAX_RECORD_LEN: usize = 1 << 20;

/// What `recover` found on disk.
pub struct Recovered {
    pub canvas: Canvas,
    /// Log records applied on top of the snapshot.
    pub replayed: usize,
    /// Bytes of a torn or corrupt log tail that were discarded.
    pub discarded_bytes: usize,
}

/// Rebuilds the canvas from `dir`, creating the directory if needed. A new
/// canvas of `width` x `height` is used when there is no snapshot; when
/// there is one, its dimensions win.
pub fn recover(dir: &Path, width: u32, height: u32) -> io::Result<Recovered> {
    fs::create_dir_all(dir)?;

    let mut canvas = match fs::read(dir.join(SNAP_FILE)) {
        Ok(bytes) => decode_snapshot(&bytes)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "canvas.snap is corrupt"))?,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Snapshot the blank canvas straight away: log records do not
            // carry the canvas size, so without this a restart with other
            // `--width`/`--height` flags would replay onto the wrong board.
            let canvas = Canvas::new(width, height);
            write_snapshot(dir, &canvas)?;
            canvas
        }
        Err(e) => return Err(e),
    };

    let log_path = dir.join(LOG_FILE);
    let log = match fs::read(&log_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };

    let mut offset = 0;
    let mut replayed = 0;
    while let Some((commit, next)) = decode_record(&log, offset) {
        if commit.seq > canvas.seq() {
            canvas.apply(&commit);
            replayed += 1;
        }
        offset = next;
    }

    let discarded_bytes = log.len() - offset;
    if discarded_bytes > 0 {
        let file = OpenOptions::new().write(true).open(&log_path)?;
        file.set_len(offset as u64)?;
        file.sync_all()?;
    }

    Ok(Recovered {
        canvas,
        replayed,
        discarded_bytes,
    })
}

/// Starts the persister thread. `replica` must be the canvas the server
/// starts from; every commit sent afterwards is logged and applied to it,
/// and every `snapshot_every` commits it is checkpointed.
pub fn spawn(
    dir: PathBuf,
    replica: Canvas,
    snapshot_every: u64,
) -> io::Result<(Sender<Commit>, JoinHandle<()>)> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG_FILE))?;
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || run(&dir, log, replica, &rx, snapshot_every.max(1)));
    Ok((tx, handle))
}

fn run(dir: &Path, log: File, mut replica: Canvas, rx: &Receiver<Commit>, snapshot_every: u64) {
    let mut log = BufWriter::new(log);
    let mut since_snapshot = 0;
    let mut record = Vec::new();

    while let Ok(first) = rx.recv() {
        // Group commit: drain whatever else is already queued so one fsync
        // covers the whole batch.
        for commit in std::iter::once(first).chain(rx.try_iter()) {
            encode_record(&commit, &mut record);
            if let Err(e) = log.write_all(&record) {
                eprintln!("persist: log write failed: {e}");
            }
            replica.apply(&commit);
            since_snapshot += 1;
        }
        if let Err(e) = log.flush().and_then(|()| log.get_ref().sync_data()) {
            eprintln!("persist: log sync failed: {e}");
        }

        if since_snapshot >= snapshot_every {
            match checkpoint(dir, &replica, log.get_mut()) {
                Ok(()) => since_snapshot = 0,
                Err(e) => eprintln!("persist: snapshot failed: {e}"),
            }
        }
    }

    // The server has gone away: leave a fresh snapshot behind.
    let _ = log.flush();
    if since_snapshot > 0
        && let Err(e) = checkpoint(dir, &replica, log.get_mut())
    {
        eprintln!("persist: final snapshot failed: {e}");
    }
}

/// Writes a snapshot of `canvas`, then empties the log it supersedes.
fn checkpoint(dir: &Path, canvas: &Canvas, log: &mut File) -> io::Result<()> {
    write_snapshot(dir, canvas)?;
    log.set_len(0)?;
    log.sync_all()
}

/// Atomically replaces the snapshot: a crash leaves either the old one or
/// the new one, never a mix.
fn write_snapshot(dir: &Path, canvas: &Canvas) -> io::Result<()> {
    let tmp = dir.join(SNAP_TMP_FILE);
    {
        let mut file = File::create(&tmp)?;
        file.write_all(&encode_snapshot(canvas))?;
        file.sync_all()?;
    }
    fs::rename(&tmp, dir.join(SNAP_FILE))?;
    // Make the rename itself durable. Not every platform can open a
    // directory for syncing, and the snapshot is valid either way.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

// ---------- encoding ----------

/// CRC-32 (IEEE), computed bitwise: records are small, so a lookup table
/// would not pay for itself.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_name(out: &mut Vec<u8>, name: &str) {
    out.push(name.len() as u8);
    out.extend_from_slice(name.as_bytes());
}

/// A bounds-checked reader over a byte slice: every getter returns `None`
/// instead of panicking when the input is short.
struct Bytes<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Bytes { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn name(&mut self) -> Option<Arc<str>> {
        let len = self.u8()?;
        let bytes = self.take(usize::from(len))?;
        std::str::from_utf8(bytes).ok().map(Arc::from)
    }

    fn colors(&mut self, n: usize) -> Option<Vec<Color>> {
        self.take(n)?
            .iter()
            .map(|&b| Color::from_index(b))
            .collect()
    }

    fn done(&self) -> bool {
        self.pos == self.buf.len()
    }
}

/// Replaces `out` with one framed log record.
fn encode_record(commit: &Commit, out: &mut Vec<u8>) {
    let mut payload = Vec::with_capacity(32 + commit.colors.len());
    put_u64(&mut payload, commit.seq);
    put_name(&mut payload, &commit.author);
    for v in [
        commit.region.col,
        commit.region.row,
        commit.region.width,
        commit.region.height,
    ] {
        put_u32(&mut payload, v);
    }
    payload.extend(commit.colors.iter().map(|c| c.index()));

    out.clear();
    put_u32(out, payload.len() as u32);
    put_u32(out, crc32(&payload));
    out.extend_from_slice(&payload);
}

/// Decodes the record at `offset`, returning it and the offset just past
/// it, or `None` if the log ends there or the record there is damaged.
fn decode_record(log: &[u8], offset: usize) -> Option<(Commit, usize)> {
    let mut header = Bytes::new(log.get(offset..)?);
    let len = header.u32()? as usize;
    let crc = header.u32()?;
    if len > MAX_RECORD_LEN {
        return None;
    }
    let payload = header.take(len)?;
    if crc32(payload) != crc {
        return None;
    }

    let mut p = Bytes::new(payload);
    let seq = p.u64()?;
    let author = p.name()?;
    let region = Rect {
        col: p.u32()?,
        row: p.u32()?,
        width: p.u32()?,
        height: p.u32()?,
    };
    let colors = p.colors(usize::try_from(region.area()).ok()?)?;
    if !p.done() {
        return None;
    }
    let commit = Commit {
        seq,
        author,
        region,
        colors,
    };
    Some((commit, offset + 8 + len))
}

fn encode_snapshot(canvas: &Canvas) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 + canvas.pixels.len() * 5);
    out.extend_from_slice(SNAP_MAGIC);
    put_u32(&mut out, canvas.width);
    put_u32(&mut out, canvas.height);
    put_u64(&mut out, canvas.seq);
    put_u32(&mut out, canvas.names.len() as u32);
    for name in &canvas.names {
        put_name(&mut out, name);
    }
    out.extend(canvas.pixels.iter().map(|c| c.index()));
    for &author in &canvas.authors {
        put_u32(&mut out, author);
    }
    let crc = crc32(&out);
    put_u32(&mut out, crc);
    out
}

fn decode_snapshot(bytes: &[u8]) -> Option<Canvas> {
    let (body, crc) = bytes.split_at_checked(bytes.len().checked_sub(4)?)?;
    if crc32(body) != u32::from_le_bytes(crc.try_into().ok()?) {
        return None;
    }

    let mut b = Bytes::new(body);
    if b.take(SNAP_MAGIC.len())? != SNAP_MAGIC {
        return None;
    }
    let width = b.u32()?;
    let height = b.u32()?;
    let seq = b.u64()?;
    let names = (0..b.u32()?)
        .map(|_| b.name())
        .collect::<Option<Vec<_>>>()?;
    let len = (width as usize).checked_mul(height as usize)?;
    let pixels = b.colors(len)?;
    let authors = (0..len).map(|_| b.u32()).collect::<Option<Vec<_>>>()?;
    if !b.done() {
        return None;
    }
    Some(Canvas::from_parts(
        width, height, seq, names, pixels, authors,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Pos;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("plaza-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn paint(canvas: &mut Canvas, author: &str, col: u32, color: &str) -> Commit {
        let region = Rect {
            col,
            row: 0,
            width: 1,
            height: 1,
        };
        canvas
            .commit(&author.into(), region, vec![Color::parse(color).unwrap()])
            .unwrap()
    }

    fn color_at(canvas: &Canvas, col: u32) -> (Color, Option<Arc<str>>) {
        canvas.read(Pos { col, row: 0 }).unwrap()
    }

    #[test]
    fn crc32_matches_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn records_round_trip() {
        let mut canvas = Canvas::new(4, 4);
        let commit = paint(&mut canvas, "ann", 2, "red");
        let mut buf = Vec::new();
        encode_record(&commit, &mut buf);
        assert_eq!(decode_record(&buf, 0), Some((commit, buf.len())));
        // Any truncation is rejected rather than misread.
        for cut in 0..buf.len() {
            assert_eq!(decode_record(&buf[..cut], 0), None);
        }
    }

    #[test]
    fn snapshot_round_trips() {
        let mut canvas = Canvas::new(3, 2);
        paint(&mut canvas, "ann", 0, "red");
        paint(&mut canvas, "bob", 1, "blue");
        let restored = decode_snapshot(&encode_snapshot(&canvas)).unwrap();
        assert_eq!(restored.seq(), 2);
        assert_eq!(color_at(&restored, 1), color_at(&canvas, 1));
        let mut bytes = encode_snapshot(&canvas);
        bytes[12] ^= 1;
        assert!(decode_snapshot(&bytes).is_none());
    }

    #[test]
    fn recovers_from_log_and_discards_torn_tail() {
        let dir = temp_dir("torn");
        let mut canvas = recover(&dir, 4, 1).unwrap().canvas;
        let (tx, handle) = spawn(dir.clone(), canvas.clone(), 1000).unwrap();
        tx.send(paint(&mut canvas, "ann", 0, "red")).unwrap();
        tx.send(paint(&mut canvas, "bob", 1, "blue")).unwrap();
        drop(tx);
        handle.join().unwrap();

        // The clean shutdown checkpointed: simulate a crash afterwards that
        // left one good record and one half-written record in the log.
        let mut log = Vec::new();
        let mut record = Vec::new();
        encode_record(&paint(&mut canvas, "cat", 2, "green"), &mut record);
        log.extend_from_slice(&record);
        let good_len = record.len() as u64;
        encode_record(&paint(&mut canvas, "dan", 3, "lime"), &mut record);
        log.extend_from_slice(&record[..record.len() - 3]);
        fs::write(dir.join(LOG_FILE), &log).unwrap();

        let recovered = recover(&dir, 4, 1).unwrap();
        assert_eq!(recovered.replayed, 1);
        assert_eq!(recovered.discarded_bytes, record.len() - 3);
        assert_eq!(recovered.canvas.seq(), 3);
        assert_eq!(color_at(&recovered.canvas, 1).1.as_deref(), Some("bob"));
        assert_eq!(color_at(&recovered.canvas, 2).1.as_deref(), Some("cat"));
        assert_eq!(color_at(&recovered.canvas, 3).1, None);
        assert_eq!(fs::metadata(dir.join(LOG_FILE)).unwrap().len(), good_len);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn checkpoints_truncate_the_log() {
        let dir = temp_dir("checkpoint");
        let mut canvas = recover(&dir, 8, 1).unwrap().canvas;
        let (tx, handle) = spawn(dir.clone(), canvas.clone(), 2).unwrap();
        for col in 0..5 {
            tx.send(paint(&mut canvas, "ann", col, "red")).unwrap();
        }
        drop(tx);
        handle.join().unwrap();

        assert_eq!(fs::metadata(dir.join(LOG_FILE)).unwrap().len(), 0);
        let recovered = recover(&dir, 8, 1).unwrap();
        assert_eq!(recovered.canvas.seq(), 5);
        assert_eq!(recovered.replayed, 0);
        fs::remove_dir_all(&dir).unwrap();
    }
}
