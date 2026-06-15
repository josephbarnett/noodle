//! Minimal batch reader for `tap.jsonl`.
//!
//! Temporary stand-in for `WireSource::FileRead` (refactor slice S13).
//! The on-disk shape is identical; when S13 lands the trait impl, this
//! module becomes a thin wrapper that delegates to the concrete impl.
//!
//! `TapEntry` in `noodle-tap::contract` only derives `Serialize`, so
//! the reader parses each JSONL line into a `serde_json::Value` and
//! exposes a typed accessor surface ([`TapEntryView`]) over it. That
//! keeps the boundary loose: new fields land on the writer side and
//! the reader handles them gracefully without a coordinated bump.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;
use thiserror::Error;

/// Errors surfaced while reading `tap.jsonl`.
#[derive(Debug, Error)]
pub enum ReadError {
    #[error("opening tap.jsonl: {0}")]
    Open(#[from] std::io::Error),

    #[error("parsing tap.jsonl line {line}: {source}")]
    Parse {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
}

/// Read every line of `tap.jsonl` into memory, returning the parsed
/// JSON value for each non-empty line.
///
/// Blank lines (whitespace-only) are skipped silently — matches the
/// `noodle-tap` writer, which never emits blank lines but a reader
/// touching a manually-edited file shouldn't choke on them.
///
/// Thin wrapper over [`crate::wire_source::FileReadSource`] (S13) — it
/// drains the batch `WireSource` to EOF. The trait impl is the single
/// definition of the on-disk read; this helper is the eager-`Vec`
/// convenience the batch callers (CLI one-shot, e2e harness) use.
pub fn read_tap_jsonl(path: &Path) -> Result<Vec<TapEntryView>, ReadError> {
    use noodle_core::WireSource;

    let mut source = crate::wire_source::FileReadSource::open(path)?;
    let mut out = Vec::new();
    while let Some(record) = source.next_record()? {
        out.push(record);
    }
    Ok(out)
}

/// A typed view over a parsed `tap.jsonl` line.
///
/// Wraps the raw `serde_json::Value` rather than deserialising into a
/// concrete struct — the wire shape on the writer side
/// (`noodle_tap::contract::TapEntry`) is `Serialize` only, and the
/// mapping layer needs unanchored access to envelope sub-blocks
/// (which the wire shape itself models as `serde_json::Value`).
#[derive(Debug, Clone)]
pub struct TapEntryView {
    raw: Value,
}

impl TapEntryView {
    /// Construct from a parsed JSON value. Mostly useful for tests
    /// that synthesise a tap record without writing to disk.
    #[must_use]
    pub fn from_value(value: Value) -> Self {
        Self { raw: value }
    }

    /// The raw underlying JSON value, for callers that need a field
    /// outside the typed accessor surface.
    #[must_use]
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// `direction` discriminator. Returns `Some("request")` /
    /// `Some("response")` for well-formed records; `None` for
    /// malformed records (which the caller can choose to skip).
    #[must_use]
    pub fn direction(&self) -> Option<&str> {
        self.raw.get("direction").and_then(Value::as_str)
    }

    /// `true` iff `direction == "request"`.
    #[must_use]
    pub fn is_request(&self) -> bool {
        self.direction() == Some("request")
    }

    /// `true` iff `direction == "response"`.
    #[must_use]
    pub fn is_response(&self) -> bool {
        self.direction() == Some("response")
    }

    /// The `event_id` ULID — pairs a request with its response.
    #[must_use]
    pub fn event_id(&self) -> Option<&str> {
        self.raw.get("event_id").and_then(Value::as_str)
    }

    /// The `timestamp` field as `RFC3339Nano` string (what the writer
    /// emits per `noodle_tap::contract`).
    #[must_use]
    pub fn timestamp(&self) -> Option<&str> {
        self.raw.get("timestamp").and_then(Value::as_str)
    }

    /// The `provider` declared by the proxy (ADR 025 §3.7).
    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.raw.get("provider").and_then(Value::as_str)
    }

    /// The request URL (request records only).
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        self.raw.get("url").and_then(Value::as_str)
    }

    /// HTTP status code (response records only).
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        self.raw
            .get("status")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
    }

    /// The `headers` map. Returns `None` when the line has no
    /// `headers` block (which is the writer's `omitempty` behaviour
    /// when no headers were observed).
    #[must_use]
    pub fn headers(&self) -> Option<&serde_json::Map<String, Value>> {
        self.raw.get("headers").and_then(Value::as_object)
    }

    /// Lookup a header by case-insensitive name. Returns the first
    /// value when present (multiple values are preserved on the wire
    /// per `BTreeMap<String, Vec<String>>`, but downstream consumers
    /// typically want the first).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let headers = self.headers()?;
        for (k, v) in headers {
            if k.eq_ignore_ascii_case(name) {
                return v.as_array()?.first()?.as_str();
            }
        }
        None
    }

    /// The `envelope` block (ADR 029 §2.4). May be absent on records
    /// the proxy hasn't enriched.
    #[must_use]
    pub fn envelope(&self) -> Option<&Value> {
        self.raw.get("envelope")
    }

    /// The `marks` block (ADR 027 §4.2 / ADR 028 §4). May be absent.
    #[must_use]
    pub fn marks(&self) -> Option<&Value> {
        self.raw.get("marks")
    }

    /// The `usage` block (ADR 029 §2.4 family 12). Populated on
    /// response records when the proxy observed token counts +/or
    /// measured latency.
    #[must_use]
    pub fn usage(&self) -> Option<&Value> {
        self.raw.get("usage")
    }

    /// The decoded body. Returns `None` when the body field is absent
    /// or `null`.
    #[must_use]
    pub fn body(&self) -> Option<&Value> {
        match self.raw.get("body") {
            Some(v) if !v.is_null() => Some(v),
            _ => None,
        }
    }
}

/// Read every line of `roundtrips.jsonl` into memory, returning the
/// parsed JSON value for each non-empty line. Same shape as
/// [`read_tap_jsonl`] but yields [`RoundTripView`]s — one
/// per-round-trip aggregated record (ADR 023 §4 / story 040.b).
///
/// Returns an empty `Vec` when the file does not exist; the
/// embellisher tolerates the absence (a tap.jsonl produced before
/// 040.b shipped won't have a sibling roundtrips.jsonl). Any other
/// I/O error propagates.
pub fn read_roundtrips_jsonl(path: &Path) -> Result<Vec<RoundTripView>, ReadError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ReadError::Open(e)),
    };
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let raw = line?;
        if raw.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(&raw).map_err(|source| ReadError::Parse {
            line: idx + 1,
            source,
        })?;
        out.push(RoundTripView { raw: value });
    }
    Ok(out)
}

/// A typed view over one parsed `roundtrips.jsonl` line. Wraps the
/// raw JSON value the same way [`TapEntryView`] does so callers can
/// read unanchored sub-blocks without a coordinated type bump on
/// every additive field.
#[derive(Debug, Clone)]
pub struct RoundTripView {
    raw: Value,
}

impl RoundTripView {
    #[must_use]
    pub fn from_value(value: Value) -> Self {
        Self { raw: value }
    }

    #[must_use]
    pub fn raw(&self) -> &Value {
        &self.raw
    }

    /// `event_id` — the join key against `tap.jsonl`.
    #[must_use]
    pub fn event_id(&self) -> Option<&str> {
        self.raw.get("event_id").and_then(Value::as_str)
    }

    /// The `attributions` map — `{category: canonical_value}` per
    /// the Resolver output. Slice 042 promotes this into the
    /// telemetry row's `context_json` so downstream consumers
    /// filter on `tool` / `work_type` directly.
    #[must_use]
    pub fn attributions(&self) -> Option<&serde_json::Map<String, Value>> {
        self.raw.get("attributions").and_then(Value::as_object)
    }

    /// `correlation` IDs (`session_id`, `turn_id`, `frame_id`) when
    /// populated. Reading these from the round-trip record lets the
    /// mapper trust the ADR 023 §2.3 contract — the fields are
    /// stamped at engine drain, not reconstructed.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.raw.get("session_id").and_then(Value::as_str)
    }

    #[must_use]
    pub fn turn_id(&self) -> Option<&str> {
        self.raw.get("turn_id").and_then(Value::as_str)
    }

    /// The frame-tree node id (ADR 052 §5). Renamed from
    /// `agent_run_id` when the marks block migrated to the frame-tree
    /// shape.
    #[must_use]
    pub fn frame_id(&self) -> Option<&str> {
        self.raw.get("frame_id").and_then(Value::as_str)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn read_empty_file_returns_empty_vec() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let out = read_tap_jsonl(tmp.path()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn read_skips_blank_lines() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            tmp,
            r#"{{"direction":"request","event_id":"a","provider":"anthropic","timestamp":"t"}}"#
        )
        .unwrap();
        writeln!(tmp).unwrap();
        writeln!(tmp, r#"{{"direction":"response","event_id":"a","provider":"anthropic","timestamp":"t","status":200}}"#).unwrap();
        let out = read_tap_jsonl(tmp.path()).unwrap();
        assert_eq!(out.len(), 2);
        assert!(out[0].is_request());
        assert!(out[1].is_response());
    }

    #[test]
    fn parse_error_reports_line_number() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, r#"{{"direction":"request"}}"#).unwrap();
        writeln!(tmp, "not json").unwrap();
        let err = read_tap_jsonl(tmp.path()).unwrap_err();
        match err {
            ReadError::Parse { line, .. } => assert_eq!(line, 2),
            ReadError::Open(_) => panic!("expected Parse error"),
        }
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let v = serde_json::json!({
            "direction": "request",
            "event_id": "a",
            "provider": "anthropic",
            "timestamp": "t",
            "headers": {
                "User-Agent": ["claude-cli/1.0"],
                "X-Stainless-Lang": ["js"]
            }
        });
        let view = TapEntryView::from_value(v);
        assert_eq!(view.header("user-agent"), Some("claude-cli/1.0"));
        assert_eq!(view.header("X-STAINLESS-LANG"), Some("js"));
        assert_eq!(view.header("missing"), None);
    }
}
