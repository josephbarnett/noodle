//! `WireSource::FileTail` — live-tail reader for `tap.jsonl`.
//!
//! The read-side dual of `crate::sink::TapJsonlLog` (ADR 027 §2.1, refactor
//! overview §2 S12). Where the sink writes the `WireSink` boundary, this
//! reads the `WireSource` boundary, in tail mode: the file is presumed
//! to be live, so `next_record` blocks waiting for the next line rather
//! than returning EOF.
//!
//! ## Position policy
//!
//! `FileTail::open` seeks to **start-of-file** by default. New consumers
//! receive everything that has already been written, then wait for new
//! records as they arrive. This matches the canonical `tail -F -n +1`
//! shape and keeps unit tests behaviour-explicit: the test writes records
//! first and then opens the reader, and it sees them.
//!
//! If callers want classic `tail -F` (only new records), construct via
//! [`FileTail::open_at_end`].
//!
//! ## Tail semantics (per `WireSource` trait docs)
//!
//! - `next_record` returns `Ok(Some(record))` once a complete JSONL line
//!   has been read and parsed.
//! - `next_record` NEVER returns `Ok(None)` — there is no EOF in tail
//!   mode. When the file is at the current write head, the reader
//!   sleeps [`Self::poll_interval`] and re-reads.
//! - `Err(_)` is returned on I/O failure or JSONL parse failure.
//!
//! ## Graceful shutdown
//!
//! [`FileTail::close`] sets an internal flag that causes the next
//! blocking poll iteration to return `Err(FileTailError::Closed)` instead
//! of looping forever. Consumers running `next_record` on a background
//! thread can use this to unblock the loop.
//!
//! ## Polling vs. fs-watch
//!
//! This v1 polls. The writer (`crate::writer`) flushes per batch and at
//! `FLUSH_INTERVAL` (100ms by default), so 50ms polling is a sympathetic
//! match — typical latency from line written to line observed is ~75ms.
//! A future revision can swap to inotify (Linux) / `FSEvents` (macOS) if
//! the latency budget tightens; the public surface stays the same.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use noodle_core::WireSource;
use serde_json::Value;
use thiserror::Error;

/// Default poll interval: see module docs for the budget rationale.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Errors surfaced by [`FileTail`].
#[derive(Debug, Error)]
pub enum FileTailError {
    /// Underlying I/O failed (read, seek, etc.).
    #[error("file_tail io: {0}")]
    Io(#[from] std::io::Error),

    /// JSONL parse failure on the line at the recorded number.
    #[error("file_tail parse on line {line}: {source}")]
    Parse {
        /// 1-based line number in the file.
        line: usize,
        #[source]
        source: serde_json::Error,
    },

    /// The reader was closed via [`FileTail::close`] while waiting.
    #[error("file_tail closed")]
    Closed,
}

/// Live-tailing `WireSource` over a `tap.jsonl` file.
///
/// `Record` is [`serde_json::Value`] (matching S16's
/// [`crate::source`]-companion reader pattern in `noodle-embellish`): the
/// writer-side [`crate::contract::TapEntry`] only derives `Serialize`, so
/// a typed `Deserialize` round-trip is not yet available without a
/// coordinated bump. Parsing into `Value` keeps the boundary loose — new
/// fields land on the writer side and the reader handles them gracefully
/// without a coordinated change.
pub struct FileTail {
    reader: BufReader<File>,
    path: PathBuf,
    /// Pending partial line (no newline yet). Concatenated across polls
    /// until the writer completes the line.
    pending: Vec<u8>,
    /// 1-based logical line counter, for parse-error messages.
    line_no: usize,
    poll_interval: Duration,
    closed: Arc<AtomicBool>,
}

impl FileTail {
    /// Open `path` and seek to start-of-file. The next [`Self::next_record`]
    /// call returns the first line that has been written.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] when the file cannot be
    /// opened (does not exist, permission denied, etc.).
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path)?;
        Ok(Self {
            reader: BufReader::new(file),
            path: path.to_path_buf(),
            pending: Vec::new(),
            line_no: 0,
            poll_interval: DEFAULT_POLL_INTERVAL,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Open `path` and seek to end-of-file. The next [`Self::next_record`]
    /// call blocks until a record is appended.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] when the file cannot be
    /// opened or seeked.
    pub fn open_at_end(path: &Path) -> std::io::Result<Self> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            reader: BufReader::new(file),
            path: path.to_path_buf(),
            pending: Vec::new(),
            line_no: 0,
            poll_interval: DEFAULT_POLL_INTERVAL,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Override the default poll interval (50ms). Lower values trade CPU
    /// for latency; higher values do the opposite.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// Path the reader is open on. Useful for diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns a clonable handle that, when [`Self::close`]'d on it,
    /// will cause the in-progress / next [`Self::next_record`] to return
    /// [`FileTailError::Closed`].
    ///
    /// Use this when the reader is owned by one thread / task and the
    /// shutdown signal needs to come from another.
    #[must_use]
    pub fn close_handle(&self) -> CloseHandle {
        CloseHandle {
            closed: Arc::clone(&self.closed),
        }
    }

    /// Signal that the reader should stop polling. The next
    /// [`Self::next_record`] iteration that hits the no-data branch
    /// returns [`FileTailError::Closed`]. Already-buffered partial lines
    /// and not-yet-flushed records may be lost — this is shutdown, not
    /// drain.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Try to read one complete line from the underlying file. Returns:
    ///
    /// - `Ok(Some(line_bytes))` — a full line is ready (no trailing `\n`).
    /// - `Ok(None)` — no full line available right now (EOF reached
    ///   before `\n`); caller should poll again.
    /// - `Err(io)` — read failed.
    fn try_take_line(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        // `BufReader::read_until` reads until the delimiter inclusive,
        // OR until EOF. So if it returns 0 bytes -> EOF with no
        // unfinished line in this call; if it returns N bytes without
        // a trailing '\n' -> EOF mid-line, must concatenate with
        // pending and try again. If it returns ending in '\n', we
        // have a complete line.
        let mut buf = Vec::new();
        let n = self.reader.read_until(b'\n', &mut buf)?;
        if n == 0 {
            // Pure EOF, no partial bytes this call.
            return Ok(None);
        }
        // Was there a newline?
        if buf.last() == Some(&b'\n') {
            // Strip the newline; prepend any pending bytes.
            buf.pop();
            // Optional CR for tolerance (TAP writer never emits but
            // we don't want to fail on a manually-edited file).
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            let mut full = std::mem::take(&mut self.pending);
            full.extend_from_slice(&buf);
            Ok(Some(full))
        } else {
            // EOF mid-line. Stash bytes; caller polls again.
            self.pending.extend_from_slice(&buf);
            Ok(None)
        }
    }
}

impl WireSource for FileTail {
    type Record = Value;
    type Error = FileTailError;

    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        loop {
            if self.is_closed() {
                return Err(FileTailError::Closed);
            }
            match self.try_take_line()? {
                Some(line_bytes) => {
                    self.line_no += 1;
                    // Skip blank lines: matches the embellish reader's
                    // tolerance for hand-edited files. The TAP writer
                    // never emits blank lines itself.
                    if line_bytes.iter().all(u8::is_ascii_whitespace) {
                        continue;
                    }
                    let value: Value = serde_json::from_slice(&line_bytes).map_err(|source| {
                        FileTailError::Parse {
                            line: self.line_no,
                            source,
                        }
                    })?;
                    return Ok(Some(value));
                }
                None => {
                    // No complete line ready. Sleep + retry.
                    thread::sleep(self.poll_interval);
                }
            }
        }
    }
}

/// Detachable shutdown signal for a [`FileTail`].
///
/// `FileTail::next_record` blocks on the calling thread; a separate
/// owner of this handle can call [`Self::close`] to break the loop.
#[derive(Debug, Clone)]
pub struct CloseHandle {
    closed: Arc<AtomicBool>,
}

impl CloseHandle {
    /// Signal the paired [`FileTail`] to stop polling at the next
    /// no-data poll iteration.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    /// A `FileTail` opened on a path that already has lines yields those
    /// lines in order on subsequent `next_record` calls.
    #[test]
    fn reads_records_already_in_file_when_opened() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::write(
            &path,
            br#"{"direction":"request","event_id":"a"}
{"direction":"response","event_id":"a"}
"#,
        )
        .unwrap();

        let mut tail = FileTail::open(&path).unwrap();
        // The reader is single-threaded blocking — so we need a
        // background closer or we'd block forever after the second
        // record. Schedule the closer ~250ms out.
        let handle = tail.close_handle();
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            handle.close();
        });

        let r1 = tail.next_record().unwrap().expect("first");
        assert_eq!(r1["event_id"], "a");
        assert_eq!(r1["direction"], "request");
        let r2 = tail.next_record().unwrap().expect("second");
        assert_eq!(r2["direction"], "response");
        // Third call: no more data; closes by signal.
        let err = tail.next_record().expect_err("expected Closed");
        match err {
            FileTailError::Closed => (),
            other => panic!("expected Closed, got {other:?}"),
        }
        stopper.join().unwrap();
    }

    /// `next_record` blocks (does not return EOF/None) when there are
    /// no records yet, and wakes when a line is appended.
    #[test]
    fn blocks_then_wakes_when_line_is_appended() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::File::create(&path).unwrap();

        let mut tail = FileTail::open(&path)
            .unwrap()
            .with_poll_interval(Duration::from_millis(10));

        // Writer thread appends after 100ms.
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            writeln!(f, r#"{{"direction":"request","event_id":"x"}}"#).unwrap();
            f.flush().unwrap();
        });

        // Also schedule a close after we've seen the record, so we
        // don't block forever.
        let close_handle = tail.close_handle();

        let start = std::time::Instant::now();
        let rec = tail.next_record().unwrap().expect("record");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(80),
            "next_record did not wait for line; elapsed {elapsed:?}"
        );
        assert_eq!(rec["event_id"], "x");

        close_handle.close();
        let _ = tail.next_record();
        writer.join().unwrap();
    }

    /// Partial line (no trailing newline) is held until the newline
    /// arrives — never parsed as a half-record.
    #[test]
    fn buffers_partial_line_until_newline_arrives() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::File::create(&path).unwrap();

        let mut tail = FileTail::open(&path)
            .unwrap()
            .with_poll_interval(Duration::from_millis(10));

        // Writer: write first half, sleep, write second half + newline.
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            f.write_all(br#"{"direction":"request","#).unwrap();
            f.flush().unwrap();
            thread::sleep(Duration::from_millis(150));
            f.write_all(b"\"event_id\":\"y\"}\n").unwrap();
            f.flush().unwrap();
        });

        let close_handle = tail.close_handle();
        let start = std::time::Instant::now();
        let rec = tail.next_record().unwrap().expect("record");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(140),
            "next_record returned before line completed; elapsed {elapsed:?}"
        );
        assert_eq!(rec["event_id"], "y");
        assert_eq!(rec["direction"], "request");

        close_handle.close();
        let _ = tail.next_record();
        writer.join().unwrap();
    }

    /// JSONL parse failure surfaces as `FileTailError::Parse` with the
    /// 1-based line number set.
    #[test]
    fn surfaces_parse_errors_with_line_number() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::write(
            &path,
            br#"{"direction":"request","event_id":"a"}
not-json
"#,
        )
        .unwrap();

        let mut tail = FileTail::open(&path).unwrap();
        let close_handle = tail.close_handle();
        // First line OK.
        let r1 = tail.next_record().unwrap().expect("first");
        assert_eq!(r1["event_id"], "a");
        // Second line: parse failure on line 2.
        let err = tail.next_record().expect_err("expected Parse");
        match err {
            FileTailError::Parse { line, .. } => assert_eq!(line, 2),
            other => panic!("expected Parse, got {other:?}"),
        }
        close_handle.close();
        let _ = tail.next_record();
    }

    /// Bursty writer: 5 records written in a short window. The reader
    /// returns each one in order.
    #[test]
    fn survives_bursty_writer() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::File::create(&path).unwrap();

        let mut tail = FileTail::open(&path)
            .unwrap()
            .with_poll_interval(Duration::from_millis(5));

        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            // Small head start so the reader gets to the poll loop.
            thread::sleep(Duration::from_millis(30));
            for i in 0..5 {
                writeln!(f, r#"{{"direction":"request","event_id":"nl-{i}"}}"#).unwrap();
                f.flush().unwrap();
            }
        });

        let close_handle = tail.close_handle();
        for i in 0..5 {
            let r = tail.next_record().unwrap().expect("record");
            assert_eq!(r["event_id"], format!("nl-{i}"));
        }
        close_handle.close();
        let _ = tail.next_record();
        writer.join().unwrap();
    }

    /// `open_at_end` skips pre-existing lines and only returns lines
    /// written AFTER opening.
    #[test]
    fn open_at_end_skips_existing_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::write(
            &path,
            br#"{"direction":"request","event_id":"old"}
"#,
        )
        .unwrap();

        let mut tail = FileTail::open_at_end(&path)
            .unwrap()
            .with_poll_interval(Duration::from_millis(10));

        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&writer_path)
                .unwrap();
            writeln!(f, r#"{{"direction":"request","event_id":"new"}}"#).unwrap();
            f.flush().unwrap();
        });

        let close_handle = tail.close_handle();
        let rec = tail.next_record().unwrap().expect("record");
        // We must get "new", not "old".
        assert_eq!(rec["event_id"], "new");
        close_handle.close();
        let _ = tail.next_record();
        writer.join().unwrap();
    }

    /// I/O error path: opening a missing file errors cleanly.
    #[test]
    fn missing_path_yields_clean_io_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        match FileTail::open(&path) {
            Ok(_) => panic!("expected NotFound error opening missing path"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        }
    }

    /// Blank lines (whitespace-only) are skipped, matching the
    /// embellish reader and the writer's never-emit-blanks policy.
    #[test]
    fn skips_blank_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::write(
            &path,
            b"\n   \n{\"direction\":\"request\",\"event_id\":\"k\"}\n",
        )
        .unwrap();

        let mut tail = FileTail::open(&path).unwrap();
        let close_handle = tail.close_handle();
        let rec = tail.next_record().unwrap().expect("record");
        assert_eq!(rec["event_id"], "k");
        close_handle.close();
        let _ = tail.next_record();
    }
}
