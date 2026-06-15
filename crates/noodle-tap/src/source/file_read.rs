//! `WireSource::FileRead` — batch reader for a finished `tap.jsonl`.
//!
//! The batch-mode dual of [`crate::source::file_tail::FileTail`] (ADR 027 §2.1,
//! refactor overview §2 S13). Where `FileTail` blocks at end-of-file waiting
//! for the next line, `FileRead` returns `Ok(None)` — it consumes a finite
//! capture from start to EOF, then stops. Repeated calls after EOF stay
//! at `Ok(None)` (idempotent EOF).
//!
//! ## Position policy
//!
//! `FileRead::open` seeks to **start-of-file**. The reader yields every
//! record in the file in write order, then signals EOF.
//!
//! ## Batch semantics (per `WireSource` trait docs)
//!
//! - `next_record` returns `Ok(Some(record))` for each complete JSONL line
//!   from start to end-of-file.
//! - `next_record` returns `Ok(None)` when the underlying read reaches
//!   true EOF without a trailing newline left to consume. The reader
//!   does NOT poll, sleep, or otherwise wait for more data — finite by
//!   contract.
//! - `Ok(None)` is idempotent: every subsequent call after the first
//!   EOF also returns `Ok(None)`.
//! - `Err(_)` is returned on I/O failure or JSONL parse failure.
//!
//! ## Error type choice
//!
//! `FileRead` uses a dedicated [`FileReadError`] rather than reusing
//! [`crate::source::file_tail::FileTailError`]. The tail variant carries
//! a `Closed` arm for graceful shutdown of its blocking poll loop;
//! batch mode never blocks and so never needs that variant. Keeping
//! the surfaces distinct avoids exposing meaningless error arms to
//! batch consumers — the same `Result` discipline that keeps
//! `WireSourceSeek` separate from `WireSource` (ADR 027 §2.1).
//!
//! ## Partial trailing line at EOF
//!
//! If the file ends mid-line (writer crash scenario, in-flight buffer
//! never flushed), the reader silently discards the partial bytes and
//! returns `Ok(None)`. This matches the broader "tolerate hand-edited
//! files" stance of the embellish reader and `FileTail`: a partial
//! trailing line is never parsed as a half-record.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use noodle_core::WireSource;
use serde_json::Value;
use thiserror::Error;

/// Errors surfaced by [`FileRead`].
#[derive(Debug, Error)]
pub enum FileReadError {
    /// Underlying I/O failed (read, etc.).
    #[error("file_read io: {0}")]
    Io(#[from] std::io::Error),

    /// JSONL parse failure on the line at the recorded number.
    #[error("file_read parse on line {line}: {source}")]
    Parse {
        /// 1-based line number in the file.
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// Batch-mode `WireSource` over a finished `tap.jsonl` file.
///
/// `Record` is [`serde_json::Value`] — same choice as
/// [`crate::source::file_tail::FileTail`] for consistency. The writer-side
/// [`crate::contract::TapEntry`] only derives `Serialize`, so a typed
/// `Deserialize` round-trip is not yet available without a coordinated
/// bump. Parsing into `Value` keeps the boundary loose: new fields land
/// on the writer side and the reader handles them gracefully without a
/// coordinated change.
pub struct FileRead {
    reader: BufReader<File>,
    path: PathBuf,
    /// 1-based logical line counter, for parse-error messages.
    line_no: usize,
    /// Latches once we hit EOF so subsequent calls stay idempotent
    /// without re-issuing read syscalls.
    eof: bool,
}

impl FileRead {
    /// Open `path` for batch reading. The next [`Self::next_record`]
    /// call returns the first line in the file.
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
            line_no: 0,
            eof: false,
        })
    }

    /// Path the reader is open on. Useful for diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl WireSource for FileRead {
    type Record = Value;
    type Error = FileReadError;

    fn next_record(&mut self) -> Result<Option<Self::Record>, Self::Error> {
        loop {
            if self.eof {
                return Ok(None);
            }
            // `BufReader::read_until` reads until the delimiter inclusive,
            // OR until EOF. So if it returns 0 bytes -> true EOF; if it
            // returns N bytes without a trailing '\n' -> EOF mid-line
            // (partial trailing line); if it returns ending in '\n', we
            // have a complete line.
            let mut buf = Vec::new();
            let n = self.reader.read_until(b'\n', &mut buf)?;
            if n == 0 {
                // True EOF, no bytes this call.
                self.eof = true;
                return Ok(None);
            }
            if buf.last() != Some(&b'\n') {
                // Partial trailing line at EOF — discard per module-doc
                // policy; never parse a half-record.
                self.eof = true;
                return Ok(None);
            }
            // Strip the newline; tolerate an optional CR (the TAP writer
            // never emits CR, but a hand-edited file shouldn't fail).
            buf.pop();
            if buf.last() == Some(&b'\r') {
                buf.pop();
            }
            self.line_no += 1;
            // Skip blank lines: matches the embellish reader's tolerance
            // for hand-edited files. The TAP writer never emits blank
            // lines itself.
            if buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value =
                serde_json::from_slice(&buf).map_err(|source| FileReadError::Parse {
                    line: self.line_no,
                    source,
                })?;
            return Ok(Some(value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    /// Reads every record in the file in order, then returns `Ok(None)`
    /// at EOF.
    #[test]
    fn reads_n_records_then_returns_eof() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::write(
            &path,
            br#"{"direction":"request","event_id":"a"}
{"direction":"response","event_id":"a"}
{"direction":"request","event_id":"b"}
{"direction":"response","event_id":"b"}
"#,
        )
        .unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        let mut collected = Vec::new();
        while let Some(rec) = rd.next_record().unwrap() {
            collected.push(rec);
        }
        assert_eq!(collected.len(), 4);
        assert_eq!(collected[0]["event_id"], "a");
        assert_eq!(collected[0]["direction"], "request");
        assert_eq!(collected[1]["direction"], "response");
        assert_eq!(collected[2]["event_id"], "b");
        assert_eq!(collected[3]["direction"], "response");
    }

    /// After the first EOF, additional calls keep returning `Ok(None)`
    /// without error. This matches the trait's batch-mode contract and
    /// the in-trait `VecSource` test in `noodle-core::wire`.
    #[test]
    fn eof_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::write(
            &path,
            br#"{"direction":"request","event_id":"only"}
"#,
        )
        .unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        let r1 = rd.next_record().unwrap().expect("first record");
        assert_eq!(r1["event_id"], "only");
        // First EOF.
        assert!(rd.next_record().unwrap().is_none(), "first EOF");
        // Subsequent calls stay at EOF.
        for _ in 0..5 {
            assert!(
                rd.next_record().unwrap().is_none(),
                "EOF must be idempotent"
            );
        }
    }

    /// Empty file: first call already returns `Ok(None)`.
    #[test]
    fn empty_file_returns_eof_immediately() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::File::create(&path).unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        assert!(rd.next_record().unwrap().is_none());
        // Still idempotent.
        assert!(rd.next_record().unwrap().is_none());
    }

    /// Partial trailing line at EOF (writer-crash scenario) is silently
    /// discarded: no parse error, no half-record yielded, EOF returned.
    #[test]
    fn partial_trailing_line_at_eof_is_discarded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        // Two complete records, then a partial third (no trailing newline).
        std::fs::write(
            &path,
            br#"{"direction":"request","event_id":"a"}
{"direction":"response","event_id":"a"}
{"direction":"request","event_"#,
        )
        .unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        let r1 = rd.next_record().unwrap().expect("first");
        assert_eq!(r1["direction"], "request");
        let r2 = rd.next_record().unwrap().expect("second");
        assert_eq!(r2["direction"], "response");
        // The partial third line is discarded — no parse error.
        assert!(rd.next_record().unwrap().is_none());
        assert!(rd.next_record().unwrap().is_none(), "EOF idempotent");
    }

    /// JSONL parse failure surfaces as `FileReadError::Parse` with the
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

        let mut rd = FileRead::open(&path).unwrap();
        let r1 = rd.next_record().unwrap().expect("first");
        assert_eq!(r1["event_id"], "a");
        let err = rd.next_record().expect_err("expected Parse");
        match err {
            FileReadError::Parse { line, .. } => assert_eq!(line, 2),
            other @ FileReadError::Io(_) => panic!("expected Parse, got {other:?}"),
        }
    }

    /// Opening a missing path surfaces the underlying `NotFound`
    /// I/O error cleanly.
    #[test]
    fn missing_path_yields_clean_io_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does-not-exist.jsonl");
        match FileRead::open(&path) {
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
            b"\n   \n{\"direction\":\"request\",\"event_id\":\"k\"}\n   \n",
        )
        .unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        let rec = rd.next_record().unwrap().expect("record");
        assert_eq!(rec["event_id"], "k");
        // Trailing blank line then EOF.
        assert!(rd.next_record().unwrap().is_none());
    }

    /// CRLF line endings are tolerated (the TAP writer never emits CR,
    /// but a hand-edited file or a Windows-side capture shouldn't fail).
    #[test]
    fn tolerates_crlf_line_endings() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::write(
            &path,
            b"{\"direction\":\"request\",\"event_id\":\"x\"}\r\n{\"direction\":\"response\",\"event_id\":\"x\"}\r\n",
        )
        .unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        let r1 = rd.next_record().unwrap().expect("first");
        assert_eq!(r1["direction"], "request");
        let r2 = rd.next_record().unwrap().expect("second");
        assert_eq!(r2["direction"], "response");
        assert!(rd.next_record().unwrap().is_none());
    }

    /// The `path()` accessor returns the path the reader was opened on.
    #[test]
    fn path_accessor_returns_open_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        std::fs::File::create(&path).unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        assert_eq!(rd.path(), path.as_path());
        // And the same after consuming to EOF.
        assert!(rd.next_record().unwrap().is_none());
        assert_eq!(rd.path(), path.as_path());
    }

    /// Many records — basic stress for the read loop. Confirms the line
    /// counter is correct on a non-trivial input.
    #[test]
    fn reads_many_records_in_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let mut file = std::fs::File::create(&path).unwrap();
        for i in 0..100 {
            writeln!(file, r#"{{"direction":"request","event_id":"nl-{i}"}}"#).unwrap();
        }
        file.flush().unwrap();

        let mut rd = FileRead::open(&path).unwrap();
        let mut collected = Vec::new();
        while let Some(rec) = rd.next_record().unwrap() {
            collected.push(rec);
        }
        assert_eq!(collected.len(), 100);
        for (i, rec) in collected.iter().enumerate() {
            assert_eq!(rec["event_id"], format!("nl-{i}"));
        }
    }
}
