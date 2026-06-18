//! OTLP HTTP/JSON exporter.
//!
//! POSTs `ResourceLogs` payloads to an `OTel` collector's
//! `/v1/logs` endpoint. Hand-encoded JSON per the OTLP/HTTP
//! specification — see [the OTLP/HTTP wire format] for the schema.
//!
//! The exporter is **stateless**: each call builds the request,
//! sends it, returns the per-row outcome. The shipper main loop
//! handles claim → export → ack as a single transactional unit.
//!
//! [the OTLP/HTTP wire format]: https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md#otlphttp

use std::time::Duration;

use serde_json::{Value, json};
use thiserror::Error;

use crate::cursor::RollupsRow;
use crate::mapping::{
    frame_agent_span, resource_attributes_for_batch, row_to_otlp_log, row_to_otlp_span,
};

/// Transport for the OTLP boundary. v1 supports HTTP/JSON only.
/// gRPC + protobuf are tracked as future work — the JSON path is
/// the lowest-friction option for plug-in collectors and covers
/// every documented OTel-Collector-Contrib build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    HttpJson,
}

/// Errors surfaced by [`OtlpExporter::export`]. Wraps the
/// `reqwest` failure modes plus a non-2xx response code as a
/// distinct variant so the shipper can distinguish "network down"
/// from "collector rejected our payload."
#[derive(Debug, Error)]
pub enum ExportError {
    #[error("building OTLP request: {0}")]
    Build(#[source] reqwest::Error),

    #[error("HTTP transport error: {0}")]
    Transport(#[source] reqwest::Error),

    #[error("collector returned {status}: {body}")]
    Status { status: u16, body: String },
}

/// Per-export outcome. The caller acks every `delivered` id and
/// fails every `failed_with` id with the error message.
#[derive(Debug, Clone)]
pub struct ExportResult {
    pub delivered: Vec<String>,
    pub failed_with: Option<String>,
}

/// Stateless OTLP HTTP/JSON exporter.
pub struct OtlpExporter {
    client: reqwest::Client,
    endpoint: String,
}

impl OtlpExporter {
    /// Build an exporter pointed at `endpoint`. The endpoint should
    /// include the host + base path; the exporter appends
    /// `/v1/logs`. For example, an `otelcol` running locally with
    /// the OTLP/HTTP receiver enabled on port 4318 is reached as
    /// `http://127.0.0.1:4318`.
    ///
    /// # Errors
    ///
    /// Returns [`ExportError::Build`] if the underlying `reqwest`
    /// client cannot be constructed (e.g. invalid TLS config).
    pub fn new(endpoint: impl Into<String>, _transport: Transport) -> Result<Self, ExportError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(ExportError::Build)?;
        Ok(Self {
            client,
            endpoint: endpoint.into(),
        })
    }

    /// Export one batch of rows. Either every row succeeds (HTTP
    /// 2xx) and all `event_id`s land in `delivered`, or the whole
    /// batch fails — the OTLP collector treats a batch atomically.
    ///
    /// # Errors
    ///
    /// Returns [`ExportError::Transport`] for network failures and
    /// [`ExportError::Status`] for non-2xx HTTP responses.
    pub async fn export(&self, rows: &[RollupsRow]) -> Result<ExportResult, ExportError> {
        if rows.is_empty() {
            return Ok(ExportResult {
                delivered: Vec::new(),
                failed_with: None,
            });
        }
        // D1.1 — ship both Log and Span payloads per batch. Logs
        // remain the canonical correlation key (off-the-shelf log
        // viewers + the noodle-viewer OTLP query tab read these);
        // spans land for distributed-tracing UIs (Phoenix, Tempo,
        // Jaeger, Honeycomb) per ADR 046 §2.3.
        self.post(rows, "/v1/logs", build_resource_logs_payload(rows))
            .await?;
        self.post(rows, "/v1/traces", build_resource_spans_payload(rows))
            .await?;
        Ok(ExportResult {
            delivered: rows.iter().map(|r| r.event_id.clone()).collect(),
            failed_with: None,
        })
    }

    async fn post(
        &self,
        _rows: &[RollupsRow],
        path: &str,
        payload: Value,
    ) -> Result<(), ExportError> {
        let url = format!("{}{path}", self.endpoint.trim_end_matches('/'));
        let response = self
            .client
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(ExportError::Transport)?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ExportError::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }
}

/// Build the OTLP/HTTP JSON envelope: one `ResourceLogs` wrapping
/// one `ScopeLogs` wrapping every `LogRecord` in the batch.
#[must_use]
pub fn build_resource_logs_payload(rows: &[RollupsRow]) -> Value {
    let log_records: Vec<Value> = rows.iter().map(row_to_otlp_log).collect();
    let resource_attrs = resource_attributes_for_batch(rows);
    json!({
        "resourceLogs": [
            {
                "resource": {
                    "attributes": resource_attrs
                },
                "scopeLogs": [
                    {
                        "scope": {
                            "name": "noodle-shipper",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "logRecords": log_records
                    }
                ]
            }
        ]
    })
}

/// D1.1 — companion of [`build_resource_logs_payload`] for the
/// `/v1/traces` endpoint. One `ResourceSpans` wrapping one
/// `ScopeSpans` wrapping the batch's spans.
///
/// ADR 057 — turn = trace, frame = `invoke_agent` span. The span set is:
///
/// - one `chat` span per round-trip ([`row_to_otlp_span`]), parented to its
///   frame's `invoke_agent` span when the row is marked;
/// - one `invoke_agent` span per `(turn_id, frame_id)` ([`frame_agent_span`]),
///   timed to bracket its round-trips and parented to the spawning frame;
/// - **side-calls** (`role == "side_call"`, off-tree per ADR 052) are emitted
///   as logs only — they carry no turn/frame and never enter the span tree.
///
/// Legacy/unmarked rows (no `turn_id`/`frame_id`) keep their flat `chat` span
/// with no parent — the pre-057 shape, preserved for back-compat.
#[must_use]
pub fn build_resource_spans_payload(rows: &[RollupsRow]) -> Value {
    use std::collections::BTreeMap;

    let mut spans: Vec<Value> = Vec::new();
    // (turn_id, frame_id) → (min start nano, max end nano, representative idx).
    let mut frames: BTreeMap<(String, String), (i64, i64, usize)> = BTreeMap::new();

    for (idx, row) in rows.iter().enumerate() {
        // Side-calls are off-tree: logs only, never a span (ADR 052 §5).
        if row.role.as_deref() == Some("side_call") {
            continue;
        }
        spans.push(row_to_otlp_span(row));

        // Accumulate the frame envelope for marked, in-tree rows.
        if let (Some(turn), Some(frame)) = (
            row.turn_id.as_deref().filter(|s| !s.is_empty()),
            row.frame_id.as_deref().filter(|s| !s.is_empty()),
        ) {
            let start = row.timestamp.saturating_mul(1_000_000);
            let end = start.saturating_add(row.latency_ms.saturating_mul(1_000_000));
            frames
                .entry((turn.to_owned(), frame.to_owned()))
                .and_modify(|e| {
                    e.0 = e.0.min(start);
                    e.1 = e.1.max(end);
                })
                .or_insert((start, end, idx));
        }
    }

    // One invoke_agent span per frame, bracketing its round-trips.
    for (start, end, idx) in frames.into_values() {
        spans.push(frame_agent_span(&rows[idx], start, end));
    }

    let resource_attrs = resource_attributes_for_batch(rows);
    json!({
        "resourceSpans": [
            {
                "resource": {
                    "attributes": resource_attrs
                },
                "scopeSpans": [
                    {
                        "scope": {
                            "name": "noodle-shipper",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "spans": spans
                    }
                ]
            }
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_row(event_id: &str) -> RollupsRow {
        RollupsRow {
            event_id: event_id.to_owned(),
            schema_id: "ai-telemetry".into(),
            schema_version: "0.0.2".into(),
            event_type: "api_call".into(),
            timestamp: 1_716_657_600_000,
            provider: "anthropic".into(),
            model: "claude-3-5-sonnet".into(),
            endpoint_path: "/v1/messages".into(),
            streaming: true,
            status_code: 200,
            error_type: None,
            latency_ms: 1500,
            input_tokens: 100,
            output_tokens: 200,
            api_key_prefix: None,
            api_key_type: None,
            session_id: Some("session-1".into()),
            session_hash: None,
            client_user_agent: None,
            agent_version: "0.0.1".into(),
            agent_arch: "aarch64".into(),
            context_json: None,
            provider_metadata_json: None,
            correlation_quality: "wire_only".into(),
            retry_count: 0,
            brain_thread_id: None,
            brain_thread_turn_index: None,
            brain_compaction_detected: None,
            brain_compaction_directive_present: None,
            brain_compaction_directive_kind: None,
            brain_blocks_dropped: None,
            brain_blocks_added: None,
            brain_estimated_window_tokens: None,
            brain_api_context_management_beta: None,
            context_input_tokens: None,
            context_cache_read_tokens: None,
            context_cache_creation_tokens: None,
            context_output_tokens: None,
            context_system_bytes: None,
            context_tools_bytes: None,
            context_tools_count: None,
            context_preamble_bytes: None,
            policy_decision: None,
            policy_mode: None,
            policy_risk: None,
            policy_rule: None,
            policy_rationale: None,
            policy_surface: None,
            turn_id: None,
            role: None,
            frame_id: None,
            parent_frame_id: None,
            depth: None,
        }
    }

    #[test]
    fn payload_carries_one_resource_logs_with_one_scope_logs() {
        let payload = build_resource_logs_payload(&[make_row("a"), make_row("b")]);
        let resource_logs = payload["resourceLogs"].as_array().unwrap();
        assert_eq!(resource_logs.len(), 1);
        let scope_logs = resource_logs[0]["scopeLogs"].as_array().unwrap();
        assert_eq!(scope_logs.len(), 1);
        let records = scope_logs[0]["logRecords"].as_array().unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn empty_batch_still_produces_well_formed_envelope() {
        let payload = build_resource_logs_payload(&[]);
        let scope_logs = payload["resourceLogs"][0]["scopeLogs"].as_array().unwrap();
        assert_eq!(scope_logs[0]["logRecords"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn scope_carries_shipper_name_and_version() {
        let payload = build_resource_logs_payload(&[make_row("a")]);
        let scope = &payload["resourceLogs"][0]["scopeLogs"][0]["scope"];
        assert_eq!(scope["name"], "noodle-shipper");
        // version is the package version; just assert it's a non-empty string.
        assert!(!scope["version"].as_str().unwrap().is_empty());
    }

    fn marked_row(event_id: &str, turn: &str, frame: &str, parent: Option<&str>) -> RollupsRow {
        let mut r = make_row(event_id);
        r.turn_id = Some(turn.to_owned());
        r.frame_id = Some(frame.to_owned());
        r.parent_frame_id = parent.map(str::to_owned);
        r.role = Some(
            if parent.is_none() {
                "main"
            } else {
                "sub_agent"
            }
            .to_owned(),
        );
        r.depth = Some(i64::from(parent.is_some()));
        r
    }

    fn span_list(payload: &Value) -> Vec<Value> {
        payload["resourceSpans"][0]["scopeSpans"][0]["spans"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// True when this span's `frame_id` attribute equals `frame`.
    fn is_frame(span: &Value, frame: &str) -> bool {
        span["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == "frame_id" && a["value"]["stringValue"] == frame)
    }

    /// ADR 057 — turn = trace, frame = `invoke_agent` span. One turn with the
    /// main frame (ROOT) + one sub-agent + a side-call. Asserts: side-call
    /// dropped, two `invoke_agent` spans, chat spans parented to their frame,
    /// the sub-agent frame parented to ROOT, all under one trace.
    #[test]
    fn span_tree_parents_chat_spans_under_invoke_agent_frames() {
        let mut side = make_row("side");
        side.role = Some("side_call".into());
        let rows = vec![
            marked_row("root-a", "turn-1", "ROOT", None),
            marked_row("root-b", "turn-1", "ROOT", None),
            marked_row("sub-a", "turn-1", "agent-7", Some("ROOT")),
            side,
        ];
        let all = span_list(&build_resource_spans_payload(&rows));

        let chat: Vec<&Value> = all.iter().filter(|s| s["kind"] == 3_i64).collect();
        let agents: Vec<&Value> = all.iter().filter(|s| s["kind"] == 1_i64).collect();
        // 3 chat spans (side-call dropped) + 2 invoke_agent spans.
        assert_eq!(chat.len(), 3, "one chat span per non-side-call round-trip");
        assert_eq!(agents.len(), 2, "one invoke_agent span per (turn, frame)");

        // One trace per turn — every span shares it.
        let trace = all[0]["traceId"].clone();
        assert!(
            all.iter().all(|s| s["traceId"] == trace),
            "one trace per turn"
        );

        let root_frame = agents.iter().find(|s| is_frame(s, "ROOT")).unwrap();
        let sub_frame = agents.iter().find(|s| is_frame(s, "agent-7")).unwrap();
        // ROOT is the trace root; the sub-agent frame parents to it.
        assert!(
            root_frame.get("parentSpanId").is_none(),
            "ROOT invoke_agent span is the trace root"
        );
        assert_eq!(
            sub_frame["parentSpanId"], root_frame["spanId"],
            "sub-agent frame ← ROOT frame"
        );

        // Each chat span parents to its own frame's invoke_agent span.
        let sub_chat = chat.iter().find(|s| is_frame(s, "agent-7")).unwrap();
        let root_chat = chat.iter().find(|s| is_frame(s, "ROOT")).unwrap();
        assert_eq!(
            sub_chat["parentSpanId"], sub_frame["spanId"],
            "sub-agent chat ← its invoke_agent frame"
        );
        assert_eq!(
            root_chat["parentSpanId"], root_frame["spanId"],
            "ROOT chat ← ROOT invoke_agent frame"
        );
    }
}
