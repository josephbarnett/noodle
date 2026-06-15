//! `TapJsonlLog` — the `WireSink` impl. Hot-path code.
//!
//! Performance contract:
//!
//! - **Disabled**: `record()` does one `Ordering::Relaxed` atomic load
//!   and returns. No allocation, no I/O.
//! - **Enabled**: `record()` serializes the JSONL line on the caller
//!   thread (~µs), then `try_send`s on a bounded mpsc channel and
//!   returns. Never blocks on file I/O.
//! - **Backpressure**: when the channel is full, `try_send` errors and
//!   we increment a `dropped` counter. The hot path keeps moving;
//!   drops are observable via [`Self::dropped_count`] (and traced
//!   once-per-N to avoid log floods).
//!
//! File I/O lives entirely in the writer task spawned by
//! [`crate::writer::spawn`]. The engine never waits on it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use noodle_core::{WireEvent, WirePatch, WireSink};

use crate::contract::{TapContent, TapDirection, TapEntry, TapEnvelope, TapPatch, body_payload};
use crate::provider::provider_from_url;
use crate::redact::redact_headers;
use crate::session::session_hash;
use crate::timestamp::format_rfc3339_nano;
use crate::writer::WriterHandle;

/// How often (in dropped events) to emit a tracing warning when the
/// channel is saturated. Keeps the log readable under sustained
/// pressure.
const DROP_LOG_PERIOD: u64 = 64;

/// Non-blocking, file-backed `WireSink` that emits TAP-compatible JSONL.
pub struct TapJsonlLog {
    enabled: AtomicBool,
    dropped: AtomicU64,
    writer: WriterHandle,
    path: PathBuf,
}

impl TapJsonlLog {
    /// Open `path` (truncating any existing file), spawn the writer
    /// task, and return a sink ready to be wrapped in `Arc`.
    ///
    /// `capacity` bounds the in-flight queue. 1024 is a sensible
    /// default for LLM workloads (~10 MiB worst case at 10 KiB/line).
    pub async fn spawn(path: PathBuf, capacity: usize) -> std::io::Result<Self> {
        let writer = crate::writer::spawn(path.clone(), capacity).await?;
        Ok(Self {
            enabled: AtomicBool::new(true),
            dropped: AtomicU64::new(0),
            writer,
            path,
        })
    }

    /// Enable or disable runtime capture without tearing down the
    /// writer task. When disabled, `record()` is a single atomic load
    /// + return (~ns).
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Trigger a graceful drain: stop accepting new events, wait for
    /// the writer task to flush, close the file. Caller is responsible
    /// for `Arc::try_unwrap`-ing first when the sink is shared.
    pub async fn shutdown(self) {
        self.writer.shutdown().await;
    }
}

impl WireSink for TapJsonlLog {
    fn record(&self, event: WireEvent) {
        // Hot-path-fast: one relaxed atomic load when disabled, no
        // allocation, no write.
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }

        let line = match build_line(&event) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!(?e, "tap sink: failed to build JSONL line");
                return;
            }
        };

        if self.writer.tx.try_send(line).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(DROP_LOG_PERIOD) {
                tracing::warn!(
                    dropped_total = n,
                    "tap sink: writer channel saturated, dropping events"
                );
            }
        }
    }

    fn record_patch(&self, patch: WirePatch) {
        // ADR 030 §4.3 / §7.3 back-patch record. Same hot-path
        // contract as `record`: drop on disabled, drop on
        // channel saturation. The patch record represents a
        // best-effort signal to consumers — losing one collapses
        // the pairing to "we observed a tool_use but no
        // resolution arrived". The proxy keeps moving.
        if !self.enabled.load(Ordering::Relaxed) {
            return;
        }
        let line = match build_patch_line(&patch) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!(?e, "tap sink: failed to build patch JSONL line");
                return;
            }
        };
        if self.writer.tx.try_send(line).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(DROP_LOG_PERIOD) {
                tracing::warn!(
                    dropped_total = n,
                    "tap sink: writer channel saturated, dropping patch records"
                );
            }
        }
    }
}

/// Convert a `WirePatch` into a TAP JSONL line per ADR 030 §7.3
/// (already terminated by `\n`). The on-disk shape is
/// `{"direction":"patch","schema_version":2,"target_request_id":"...",
///   "timestamp":"...","patches":[{"path":"...","value":...}]}`.
fn build_patch_line(patch: &WirePatch) -> Result<Vec<u8>, serde_json::Error> {
    let entry = TapPatch::from_wire(patch);
    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');
    Ok(line)
}

/// Convert a `WireEvent` into a TAP JSONL line (already terminated by
/// `\n`).
fn build_line(event: &WireEvent) -> Result<Vec<u8>, serde_json::Error> {
    let content_type = lookup_header(event, "content-type");
    let body_in_value = body_payload(&event.body_in, content_type.as_deref());
    // Only surface `body_out` when noodle actually mutated this
    // direction. Equal bytes ⇒ omit, keeping passthrough exchanges
    // single-bodied on disk and making mutations visible at a
    // glance (TapEntry.body_out present ⇔ enhancement or strip
    // happened on this side).
    let body_out_value = if event.body_out == event.body_in {
        None
    } else {
        Some(body_payload(&event.body_out, content_type.as_deref()))
    };
    let direction = TapDirection::from(event.direction);
    let (method, url) = if matches!(direction, TapDirection::Request) {
        (
            event.method.as_ref().map(ToString::to_string),
            derive_full_url(event),
        )
    } else {
        (None, None)
    };
    let status = if matches!(direction, TapDirection::Response) {
        event.status
    } else {
        None
    };
    let entry = TapEntry {
        direction,
        timestamp: format_rfc3339_nano(event.ts_unix_ms),
        event_id: event.request_id.to_string(),
        // Per ADR 025 §3.7: prefer the cell-declared provider when
        // the proxy stamped one upstream. Fall back to host-suffix
        // derivation for cells that don't yet ship a declared
        // provider — this keeps existing tap.jsonl readers happy
        // during the migration.
        provider: event.provider.as_ref().map_or_else(
            || derive_provider(event).to_owned(),
            smol_str::SmolStr::to_string,
        ),
        method,
        url,
        status,
        // Session hash keys on the bytes the client sent us
        // (body_in), so a session id stays stable whether or not
        // noodle mutated. Mutation should not collapse two distinct
        // client sessions.
        session_hash: session_hash(&event.headers, &event.body_in),
        headers: redact_headers(&event.headers),
        body: body_in_value,
        body_out: body_out_value,
        marks: event.marks.as_ref().map(crate::contract::TapMarks::from),
        // ADR 029 §2.4 envelope-level operational-context block
        // (refactor slices S6 + S7). The proxy stamps the four
        // sub-fields as pre-serialized `serde_json::Value`s — the
        // sink embeds them under `envelope.*` verbatim. When the
        // proxy hasn't stamped any of the four, the envelope
        // block is omitted entirely (the JSONL line stays
        // byte-identical to pre-S6).
        envelope: TapEnvelope::from_wire(
            event.agent_app.as_ref(),
            event.machine.as_ref(),
            event.collector_app.as_ref(),
            event.subscription.as_ref(),
        ),
        // ADR 027 / ADR 029 §2.4 / refactor overview §2 S8:
        // populate `usage.tokens` (from `message_delta.usage`)
        // and `usage.latency` (from request-send → first-byte and
        // request-send → response-close) on response records.
        // Request records always come through with
        // `event.usage = None` from the wirelog hot path.
        usage: event.usage.as_ref().map(crate::contract::TapUsage::from),
        // ADR 030 §2 / refactor overview §2 S9: decoded
        // `content.blocks[]` on response records. The proxy
        // accumulates typed `text` / `thinking` / `tool_use`
        // blocks across the SSE stream and stamps the array on
        // `event.content_blocks`; the sink wraps that into
        // `TapContent { blocks: ... }` so the on-disk shape is
        // `content.blocks[]` exactly as ADR 030 §2.1 specifies.
        content: TapContent::from_wire(event.content_blocks.as_ref()),
        // ADR 030 §3 / refactor overview §2 S10: parsed SSE
        // event stream on response records. The proxy
        // accumulates per-event `{ts_offset_ms, type, ...payload}`
        // entries across the SSE stream and stamps the array on
        // `event.events`; the sink embeds the array verbatim
        // under `events[]` on disk per ADR 030 §3.1.
        events: event.events.clone(),
        // ADR 030 §4 / refactor overview §2 S11: tool-use
        // cross-record pairing. On request records the proxy
        // stamps the back-reference (`resolves_tool_use_in_request_id`)
        // when a `tool_result` matches a pending tool_use; on
        // response records the forward reference is delivered
        // out-of-band via patch records per ADR 030 §7.3. The
        // sink decodes the proxy's pre-serialized JSON value
        // into the on-disk typed shape.
        pairing: crate::contract::TapPairing::from_wire(event.pairing.as_ref()),
        attribution: event.attribution.clone(),
    };
    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');
    Ok(line)
}

/// Reconstruct the full request URL. HTTPS-MITM'd HTTP/2 requests
/// arrive with a path-only URI; the proxy proxies them by reading the
/// `:authority` header into the request URI but rama collapses that
/// back to a path on the inner side. We rebuild
/// `https://{host}{path}` from the request `Host` header.
fn derive_full_url(event: &WireEvent) -> Option<String> {
    let raw = event.url.as_deref()?;
    if raw.contains("://") {
        // Already a full URL (e.g. plain-HTTP forward-proxy requests
        // where curl sends `POST http://example.com/foo HTTP/1.1`).
        return Some(raw.to_owned());
    }
    let host = lookup_header(event, "host")?;
    // Assume HTTPS for path-only requests — that's the MITM case. The
    // plain-HTTP forward path emits full URLs above, so we don't
    // misattribute http→https there.
    let path = if raw.starts_with('/') { raw } else { "/" };
    Some(format!("https://{host}{path}"))
}

/// Provider derivation prefers the URL when it carries a scheme +
/// authority. Falls back to the `Host` request header — which is what
/// `parts.uri.to_string()` collapses to for HTTPS-MITM'd HTTP/2
/// requests (path-only URI, host in the header).
fn derive_provider(event: &WireEvent) -> &'static str {
    let url = event.url.as_deref().unwrap_or("");
    if url.contains("://") {
        return provider_from_url(url);
    }
    if let Some(host) = lookup_header(event, "host") {
        return provider_from_url(&host);
    }
    "unknown"
}

fn lookup_header(event: &WireEvent, name: &str) -> Option<String> {
    event
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use noodle_core::{HeaderPair, WireDirection, WireEvent};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn anthropic_request_event() -> WireEvent {
        anthropic_event_with_url(Some("https://api.anthropic.com/v1/messages".into()))
    }

    fn anthropic_event_with_url(url: Option<String>) -> WireEvent {
        // Derived from a literal datetime so the assertion below isn't
        // coupled to mental arithmetic.
        let dt = time::macros::datetime!(2026-05-10 17:08:59.123 UTC);
        let ts_ms = u64::try_from(dt.unix_timestamp_nanos() / 1_000_000).unwrap();
        WireEvent {
            direction: WireDirection::Request,
            request_id: "nl-1".into(),
            ts_unix_ms: ts_ms,
            method: Some("POST".into()),
            url,
            status: None,
            headers: vec![
                HeaderPair {
                    name: "Content-Type".into(),
                    value: "application/json".into(),
                },
                HeaderPair {
                    name: "Host".into(),
                    value: "api.anthropic.com".into(),
                },
                HeaderPair {
                    name: "X-Api-Key".into(),
                    value: "sk-ant-secret-12345".into(),
                },
                HeaderPair {
                    name: "X-Claude-Code-Session-Id".into(),
                    value: "sess-abc-123".into(),
                },
            ],
            body_in: Bytes::from_static(br#"{"model":"claude-haiku-4-5","messages":[]}"#),
            body_out: Bytes::from_static(br#"{"model":"claude-haiku-4-5","messages":[]}"#),
            marks: None,
            provider: None,
            agent_app: None,
            machine: None,
            collector_app: None,
            subscription: None,
            usage: None,
            content_blocks: None,
            events: None,
            pairing: None,
            attribution: None,
        }
    }

    #[test]
    fn build_line_emits_expected_shape() {
        let line = build_line(&anthropic_request_event()).expect("build");
        assert!(line.ends_with(b"\n"));
        let v: serde_json::Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(v["direction"], "request");
        assert_eq!(v["event_id"], "nl-1");
        assert_eq!(v["provider"], "anthropic");
        assert_eq!(v["timestamp"], "2026-05-10T17:08:59.123Z");
        assert_eq!(v["session_hash"], "sess-abc-123");
        // method + url present on the request side
        assert_eq!(v["method"], "POST");
        assert_eq!(v["url"], "https://api.anthropic.com/v1/messages");
        // status omitted on the request side
        assert!(v.get("status").is_none());
        // x-api-key redacted per ADR 027 §9 prefix-preserving
        // policy (N=12, marker = `...<redacted>`).
        assert_eq!(v["headers"]["X-Api-Key"][0], "sk-ant-secre...<redacted>");
        assert_eq!(v["headers"]["Content-Type"][0], "application/json");
        // body parsed as object
        assert_eq!(v["body"]["model"], "claude-haiku-4-5");
    }

    #[test]
    fn build_line_reconstructs_url_from_host_when_path_only() {
        // HTTPS-MITM'd HTTP/2 requests arrive with URI = "/v1/messages".
        let mut ev = anthropic_event_with_url(Some("/v1/messages".into()));
        ev.method = Some("POST".into());
        let line = build_line(&ev).expect("build");
        let v: serde_json::Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(v["url"], "https://api.anthropic.com/v1/messages");
        assert_eq!(v["provider"], "anthropic");
    }

    #[test]
    fn build_line_carries_usage_block_when_event_has_usage() {
        // S8: when the wirelog stamped `WireUsage` on the
        // response event, the resulting tap.jsonl line must
        // carry the on-disk usage shape with the canonical field
        // names (`input_tokens`, `output_tokens`, etc.).
        let mut ev = anthropic_event_with_url(Some("/v1/messages".into()));
        ev.direction = WireDirection::Response;
        ev.method = None;
        ev.status = Some(200);
        ev.usage = Some(noodle_core::WireUsage {
            tokens: Some(noodle_core::WireTokenUsage {
                input: 12,
                output: 256,
                cached_read: Some(1024),
                cached_creation: Some(0),
                reasoning: None,
                cache_creation: None,
                vendor_extras: std::collections::BTreeMap::new(),
            }),
            latency: Some(noodle_core::WireLatency {
                time_to_first_byte_ms: Some(42),
                total_ms: Some(987),
            }),
            service_tier: None,
            inference_geo: None,
        });
        let line = build_line(&ev).expect("build");
        let v: serde_json::Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(v["usage"]["tokens"]["input_tokens"], 12);
        assert_eq!(v["usage"]["tokens"]["output_tokens"], 256);
        assert_eq!(v["usage"]["tokens"]["cache_read_input_tokens"], 1024);
        assert_eq!(v["usage"]["latency"]["time_to_first_byte_ms"], 42);
        assert_eq!(v["usage"]["latency"]["total_ms"], 987);
    }

    #[test]
    fn build_line_omits_usage_block_when_event_has_no_usage() {
        let ev = anthropic_request_event();
        let line = build_line(&ev).expect("build");
        let s = std::str::from_utf8(&line).unwrap();
        assert!(!s.contains("usage"), "usage absent on the wire: {s}");
    }

    #[test]
    fn build_line_response_carries_status_not_method_or_url() {
        let mut ev = anthropic_event_with_url(Some("/v1/messages".into()));
        ev.direction = WireDirection::Response;
        ev.method = None;
        ev.status = Some(401);
        let line = build_line(&ev).expect("build");
        let v: serde_json::Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(v["direction"], "response");
        assert_eq!(v["status"], 401);
        assert!(v.get("method").is_none(), "method elided on response");
        assert!(v.get("url").is_none(), "url elided on response");
    }

    #[test]
    fn envelope_fields_round_trip_through_build_line() {
        // ADR 029 §2.4 — the three envelope fields the proxy
        // stamps on every `WireEvent` must surface on the JSONL
        // record under `envelope.agent_app`, `envelope.machine`,
        // `envelope.collector_app`. Tests at the `build_line`
        // boundary so the sink contract is pinned even before
        // the proxy stamps it for real.
        let mut ev = anthropic_request_event();
        ev.agent_app = Some(serde_json::json!({
            "name": "claude_code",
            "version": null,
            "build_hash": null,
            "build_date": null,
            "source": "user_agent_header",
        }));
        ev.machine = Some(serde_json::json!({
            "hostname": "ci-host",
            "os_family": "linux",
            "os_version": null,
            "architecture": "x86_64",
            "locale": null,
            "timezone": null,
        }));
        ev.collector_app = Some(serde_json::json!({
            "name": "noodle",
            "version": "0.0.1",
            "build_hash": "abc1234",
            "build_date": "2026-05-21T00:00:00Z",
            "features": ["tap"],
        }));
        let line = build_line(&ev).expect("build");
        let v: serde_json::Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert_eq!(v["envelope"]["agent_app"]["name"], "claude_code");
        assert_eq!(v["envelope"]["machine"]["os_family"], "linux");
        assert_eq!(v["envelope"]["collector_app"]["name"], "noodle");
        assert_eq!(v["envelope"]["collector_app"]["build_hash"], "abc1234");
    }

    #[test]
    fn envelope_block_omitted_when_proxy_did_not_stamp() {
        // Passthrough: the proxy didn't stamp envelope fields.
        // The JSONL line stays byte-identical to pre-S6 — no
        // `envelope` key on disk.
        let line = build_line(&anthropic_request_event()).expect("build");
        let v: serde_json::Value = serde_json::from_slice(&line[..line.len() - 1]).unwrap();
        assert!(v.get("envelope").is_none());
    }

    #[tokio::test]
    async fn disabled_sink_writes_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 4).await.unwrap());
        sink.set_enabled(false);
        sink.record(anthropic_request_event());
        // Give the writer task a chance to do nothing.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.is_empty(), "disabled sink wrote: {s:?}");
        Arc::try_unwrap(sink).ok().unwrap().shutdown().await;
    }

    #[tokio::test]
    async fn enabled_sink_writes_jsonl_to_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 4).await.unwrap());
        sink.record(anthropic_request_event());

        // Drain to force flush, via shutdown.
        Arc::try_unwrap(sink)
            .ok()
            .expect("unique arc")
            .shutdown()
            .await;

        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.starts_with('{') && s.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["event_id"], "nl-1");
        assert_eq!(v["provider"], "anthropic");
    }

    #[tokio::test]
    async fn record_patch_writes_patch_line_to_file() {
        // S11 sink contract: a back-patch record emitted via
        // `record_patch` lands on tap.jsonl as a sibling line
        // with `direction: "patch"` and the target-id pointer.
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 8).await.unwrap());
        sink.record(anthropic_request_event());
        sink.record_patch(noodle_core::WirePatch {
            target_request_id: "nl-1".into(),
            ts_unix_ms: 1_700_000_000_000,
            patches: vec![noodle_core::WirePatchEntry {
                path: "pairing.resolved_by_request_id".into(),
                value: serde_json::Value::String("nl-2".into()),
            }],
        });
        Arc::try_unwrap(sink).ok().unwrap().shutdown().await;

        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "regular + patch line");

        let entry: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry["direction"], "request");
        assert_eq!(entry["event_id"], "nl-1");

        let patch_value: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(patch_value["direction"], "patch");
        assert_eq!(patch_value["target_request_id"], "nl-1");
        assert_eq!(
            patch_value["patches"][0]["path"],
            "pairing.resolved_by_request_id"
        );
        assert_eq!(patch_value["patches"][0]["value"], "nl-2");
    }

    #[tokio::test]
    async fn record_patch_is_noop_when_disabled() {
        // The same disabled-fast-path discipline as `record`.
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let sink = Arc::new(TapJsonlLog::spawn(path.clone(), 4).await.unwrap());
        sink.set_enabled(false);
        sink.record_patch(noodle_core::WirePatch {
            target_request_id: "nl-1".into(),
            ts_unix_ms: 0,
            patches: vec![noodle_core::WirePatchEntry {
                path: "pairing.resolved_by_request_id".into(),
                value: serde_json::Value::String("nl-2".into()),
            }],
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let s = std::fs::read_to_string(&path).unwrap();
        assert!(s.is_empty(), "disabled sink wrote: {s:?}");
        Arc::try_unwrap(sink).ok().unwrap().shutdown().await;
    }
}
