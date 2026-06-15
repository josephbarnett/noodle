//! File-backed [`WireSource`] implementations for `tap.jsonl`.
//!
//! Realises refactor slices **S13** ([`FileReadSource`], batch) and
//! **S12** ([`FileTailSource`], tail) — the file-backed read-side duals
//! of `noodle_tap::WireSink::File`. Both yield [`TapEntryView`]s, the
//! loose JSON-value view over the `Serialize`-only
//! `noodle_tap::contract::TapEntry`.
//!
//! - [`FileReadSource`] reads an existing capture to EOF, then returns
//!   `Ok(None)` — finite/batch semantics.
//! - [`FileTailSource`] follows a live `tap.jsonl` the proxy is
//!   appending to: `next_record` blocks at EOF until the next *complete*
//!   line arrives (or a stop flag is set). It never parses a trailing
//!   partial line — a record the proxy is mid-write on stays unread
//!   until its terminating newline lands.
//!
//! This is the consumption seam the rest of the system reads through:
//! the proxy writes records via `WireSink`; the embellish processor
//! (continuous, in tail mode) and — per ADR 044 — the `ParquetSink` read
//! them back via `WireSource`. Landing the trait impls here (rather than
//! a bespoke file reader per consumer) is what lets those consumers
//! stack on one boundary.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use noodle_core::{WireSource, WireSourceSeek};
use serde_json::Value;

use crate::reader::{ReadError, TapEntryView};

/// Batch [`WireSource`] over an existing `tap.jsonl` (refactor S13).
///
/// Yields one [`TapEntryView`] per non-blank line in on-disk order,
/// then `Ok(None)` at EOF. A malformed line surfaces as
/// `Err(ReadError::Parse)`; per the [`WireSource`] contract the caller
/// may log and continue calling `next_record`.
pub struct FileReadSource {
    reader: BufReader<File>,
    line_no: usize,
}

impl FileReadSource {
    /// Open `path` for batch reading. The file is read lazily, one line
    /// per [`WireSource::next_record`] call.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError::Open`] if the file cannot be opened.
    pub fn open(path: &Path) -> Result<Self, ReadError> {
        let file = File::open(path)?;
        Ok(Self {
            reader: BufReader::new(file),
            line_no: 0,
        })
    }
}

impl WireSource for FileReadSource {
    type Record = TapEntryView;
    type Error = ReadError;

    fn next_record(&mut self) -> Result<Option<TapEntryView>, ReadError> {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line)?;
            if n == 0 {
                return Ok(None); // EOF — batch semantics.
            }
            self.line_no += 1;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue; // skip blank lines (matches the writer's tolerance).
            }
            let value: Value =
                serde_json::from_str(trimmed).map_err(|source| ReadError::Parse {
                    line: self.line_no,
                    source,
                })?;
            return Ok(Some(TapEntryView::from_value(value)));
        }
    }
}

// `FileReadSource` deliberately does not implement `WireSourceSeek`:
// the batch path reads straight to EOF and never rewinds, and
// `WireSourceSeek` is the optional companion trait (the `io::Read` /
// `io::Seek` split) precisely so finite sources need not invent a
// position-tracking story. `FileTailSource` — which checkpoints — does
// implement it.

/// Tail [`WireSource`] over a live `tap.jsonl` (refactor S12).
///
/// Follows a file the proxy is appending to. [`WireSource::next_record`]
/// blocks at EOF — sleeping `poll_interval` and retrying — until the
/// next complete line is available, returning `Ok(None)` only when the
/// `stop` flag is set (clean shutdown / test drain). It tracks a byte
/// offset so a restart resumes where it left off
/// ([`WireSourceSeek`]); because the downstream `SQLite` writer is
/// idempotent on `event_id`, a restart that replays from 0 is also
/// safe.
///
/// ## Partial-line safety
///
/// Only newline-terminated lines are consumed. A record the proxy is
/// mid-write on (no trailing `\n` yet) stays unread until the newline
/// arrives, so the tail never parses a half-written JSON line.
///
/// ## Rotation / truncation
///
/// If the file shrinks below the current offset (truncation or
/// rotation), the source resets to offset 0 and replays from the top.
pub struct FileTailSource {
    path: PathBuf,
    offset: u64,
    record_no: usize,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
    buf: VecDeque<TapEntryView>,
}

impl FileTailSource {
    /// Tail `path` from the beginning. `poll_interval` is how long
    /// `next_record` sleeps between EOF retries; `stop` lets a caller
    /// break the tail cleanly (`next_record` then returns `Ok(None)`).
    #[must_use]
    pub fn new(path: PathBuf, poll_interval: Duration, stop: Arc<AtomicBool>) -> Self {
        Self {
            path,
            offset: 0,
            record_no: 0,
            poll_interval,
            stop,
            buf: VecDeque::new(),
        }
    }

    /// Non-blocking read of every *complete* line from the current
    /// offset to EOF. Parsed records are appended to the internal
    /// buffer and the offset advances past the last consumed newline.
    /// A trailing partial line is left for a later call. Malformed lines
    /// are logged and skipped (the offset still advances past them, so
    /// they are not retried). Returns the number of records buffered.
    ///
    /// # Errors
    ///
    /// Returns [`ReadError::Open`] on an I/O failure other than the file
    /// not existing yet (treated as "nothing to read").
    pub fn poll(&mut self) -> Result<usize, ReadError> {
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e.into()),
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            // Truncated / rotated — replay from the top.
            self.offset = 0;
        }
        if len == self.offset {
            return Ok(0);
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        let Some(last_nl) = bytes.iter().rposition(|&b| b == b'\n') else {
            return Ok(0); // no complete line yet — leave the partial line.
        };
        let complete = &bytes[..=last_nl];

        let mut added = 0;
        for raw in complete.split(|&b| b == b'\n') {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue; // blank line between records.
            }
            self.record_no += 1;
            match serde_json::from_slice::<Value>(raw) {
                Ok(value) => {
                    self.buf.push_back(TapEntryView::from_value(value));
                    added += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        line = self.record_no,
                        %error,
                        "skipping malformed tap.jsonl line in tail",
                    );
                }
            }
        }
        // Advance past everything we consumed (including any skipped
        // malformed lines) so they are not retried.
        self.offset += (last_nl as u64) + 1;
        Ok(added)
    }

    /// Poll once and drain all currently-buffered records into a `Vec`,
    /// without blocking. The driver loop uses this so it can interleave
    /// other work (refreshing the roundtrips index) between batches.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::poll`] errors.
    pub fn poll_batch(&mut self) -> Result<Vec<TapEntryView>, ReadError> {
        self.poll()?;
        Ok(self.buf.drain(..).collect())
    }
}

impl WireSource for FileTailSource {
    type Record = TapEntryView;
    type Error = ReadError;

    fn next_record(&mut self) -> Result<Option<TapEntryView>, ReadError> {
        loop {
            if let Some(record) = self.buf.pop_front() {
                return Ok(Some(record));
            }
            self.poll()?;
            if !self.buf.is_empty() {
                continue;
            }
            if self.stop.load(Ordering::Relaxed) {
                return Ok(None); // clean shutdown — not EOF.
            }
            std::thread::sleep(self.poll_interval);
        }
    }
}

impl WireSourceSeek for FileTailSource {
    fn seek(&mut self, offset: u64) -> Result<(), ReadError> {
        self.offset = offset;
        self.buf.clear();
        Ok(())
    }

    fn current_offset(&self) -> Result<u64, ReadError> {
        Ok(self.offset)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn req(event_id: &str) -> String {
        format!(r#"{{"direction":"request","event_id":"{event_id}","provider":"anthropic"}}"#)
    }
    fn resp(event_id: &str) -> String {
        format!(r#"{{"direction":"response","event_id":"{event_id}","status":200}}"#)
    }

    #[test]
    fn file_read_yields_every_record_then_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tap.jsonl");
        std::fs::write(&path, format!("{}\n{}\n", req("a"), resp("a"))).expect("write");

        let mut src = FileReadSource::open(&path).expect("open");
        assert_eq!(src.next_record().unwrap().unwrap().event_id(), Some("a"));
        assert_eq!(src.next_record().unwrap().unwrap().event_id(), Some("a"));
        assert!(
            src.next_record().unwrap().is_none(),
            "batch source ends at EOF"
        );
    }

    #[test]
    fn file_read_skips_blank_lines_and_errors_on_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tap.jsonl");
        std::fs::write(&path, format!("{}\n\n  \nnot json\n", req("a"))).expect("write");

        let mut src = FileReadSource::open(&path).expect("open");
        assert_eq!(src.next_record().unwrap().unwrap().event_id(), Some("a"));
        // blank lines skipped; the "not json" line surfaces as a parse error.
        assert!(matches!(src.next_record(), Err(ReadError::Parse { .. })));
    }

    #[test]
    fn file_tail_resumes_across_appends_by_offset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tap.jsonl");
        std::fs::write(&path, format!("{}\n", req("a"))).expect("write");

        let stop = Arc::new(AtomicBool::new(false));
        let mut tail = FileTailSource::new(path.clone(), Duration::from_millis(5), stop.clone());

        // First poll sees the one existing record.
        let batch = tail.poll_batch().expect("poll");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].event_id(), Some("a"));
        let after_first = tail.current_offset().unwrap();

        // Append more while "the proxy keeps writing" — only the new
        // records come back, proving offset resumption.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        write!(f, "{}\n{}\n", resp("a"), req("b")).expect("write");
        f.flush().unwrap();

        let batch = tail.poll_batch().expect("poll");
        let ids: Vec<_> = batch.iter().filter_map(|r| r.event_id()).collect();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(tail.current_offset().unwrap() > after_first);
    }

    #[test]
    fn file_tail_does_not_consume_a_partial_trailing_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tap.jsonl");
        // One complete line + a partial line with no terminating newline.
        std::fs::write(&path, format!("{}\n{{\"direction\":\"resp", req("a"))).expect("write");

        let stop = Arc::new(AtomicBool::new(false));
        let mut tail = FileTailSource::new(path.clone(), Duration::from_millis(5), stop);

        let batch = tail.poll_batch().expect("poll");
        assert_eq!(batch.len(), 1, "only the complete line is consumed");

        // Complete the partial line; now it becomes available.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        writeln!(f, "onse\",\"event_id\":\"a\",\"status\":200}}").expect("write");
        f.flush().unwrap();

        let batch = tail.poll_batch().expect("poll");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].event_id(), Some("a"));
    }

    #[test]
    fn file_tail_resets_on_truncation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tap.jsonl");
        std::fs::write(&path, format!("{}\n{}\n", req("a"), resp("a"))).expect("write");

        let stop = Arc::new(AtomicBool::new(false));
        let mut tail = FileTailSource::new(path.clone(), Duration::from_millis(5), stop);
        assert_eq!(tail.poll_batch().unwrap().len(), 2);

        // Truncate + rewrite (rotation). The source should replay from 0.
        std::fs::write(&path, format!("{}\n", req("z"))).expect("truncate");
        let batch = tail.poll_batch().expect("poll");
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].event_id(), Some("z"));
    }

    #[test]
    fn file_tail_next_record_returns_none_when_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tap.jsonl");
        std::fs::write(&path, format!("{}\n", req("a"))).expect("write");

        let stop = Arc::new(AtomicBool::new(false));
        let mut tail = FileTailSource::new(path, Duration::from_millis(5), stop.clone());

        assert_eq!(tail.next_record().unwrap().unwrap().event_id(), Some("a"));
        // No more data + stop set → clean end rather than an infinite block.
        stop.store(true, Ordering::Relaxed);
        assert!(tail.next_record().unwrap().is_none());
    }

    #[test]
    fn file_tail_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist-yet.jsonl");
        let stop = Arc::new(AtomicBool::new(false));
        let mut tail = FileTailSource::new(path, Duration::from_millis(5), stop);
        assert_eq!(
            tail.poll_batch().unwrap().len(),
            0,
            "absent file polls empty"
        );
    }
}
