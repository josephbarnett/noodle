//! `ai-telemetry` v0.0.2 row → OTLP Log record (HTTP/JSON encoding).
//!
//! The OTLP/HTTP JSON wire format is documented at
//! <https://github.com/open-telemetry/opentelemetry-specification/blob/main/specification/protocol/otlp.md#otlphttp>
//! and in [`opentelemetry-proto v1.5.0`]. We hand-encode to JSON
//! rather than depending on `opentelemetry-otlp` because the
//! shipper's needs (one resource log per batch, no tracer SDK, no
//! gRPC) make the dep's surface area overkill.
//!
//! ## Placement strategy (per E4 §B caveat catalogue)
//!
//! - `session_id` + `frame_id` ride at **resource scope** —
//!   stable across many records from the same agent run.
//! - `event_id` + `turn_id` + `flow_id` ride at **record scope**
//!   (`attributes`) — vary per event.
//! - `flow_id` is encoded as a **stringValue**, not intValue.
//!   Rust's `u64` would overflow OTLP's signed int64.
//!
//! [`opentelemetry-proto v1.5.0`]: https://github.com/open-telemetry/opentelemetry-proto/releases/tag/v1.5.0

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::cursor::RollupsRow;

/// Severity number for an `api_call` log record. v1 uses `INFO` (9)
/// across the board; future event types may differentiate (errors
/// → `ERROR`).
const SEVERITY_INFO: i64 = 9;

/// Build one OTLP `LogRecord` JSON object from a rollups row.
///
/// Returns a `serde_json::Value` (Object) rather than a string so
/// the exporter can collect many records into one
/// `ResourceLogs.ScopeLogs.logRecords` array per HTTP POST.
#[must_use]
#[allow(clippy::too_many_lines)] // attribute set grew with brain.* + gen_ai.* additions; splitting hurts readability more than the linter helps
pub fn row_to_otlp_log(row: &RollupsRow) -> Value {
    let mut attrs = Map::new();

    // ── correlation (record scope) ────────────────────────────────
    attrs.insert("event_id".into(), kv_str(&row.event_id));
    if let Some(ref s) = row.session_id {
        attrs.insert("session_id".into(), kv_str(s));
    }
    if let Some(ref s) = row.session_hash {
        attrs.insert("session_hash".into(), kv_str(s));
    }
    attrs.insert(
        "correlation_quality".into(),
        kv_str(&row.correlation_quality),
    );

    // ── request envelope ─────────────────────────────────────────
    attrs.insert("provider".into(), kv_str(&row.provider));
    attrs.insert("model".into(), kv_str(&row.model));
    attrs.insert("endpoint_path".into(), kv_str(&row.endpoint_path));
    attrs.insert("streaming".into(), kv_bool(row.streaming));
    attrs.insert("status_code".into(), kv_int(row.status_code));
    if let Some(ref e) = row.error_type {
        attrs.insert("error_type".into(), kv_str(e));
    }
    attrs.insert("latency_ms".into(), kv_int(row.latency_ms));

    // ── usage ────────────────────────────────────────────────────
    attrs.insert("input_tokens".into(), kv_int(row.input_tokens));
    attrs.insert("output_tokens".into(), kv_int(row.output_tokens));

    // ── OTel GenAI semantic conventions (ADR 046 §2.3) ───────────
    // Emit standard gen_ai.* names alongside our existing attrs so
    // off-the-shelf OTel GenAI viewers (otel-tui, Phoenix, Grafana
    // GenAI dashboards) render captured sessions natively with no
    // noodle-side UI. Names follow the OTel Semantic Conventions
    // GenAI spec; only attributes we can populate from the row are
    // emitted (the spec also defines opt-in fields for prompt/
    // completion bodies and tool-call payloads — those are deferred
    // until a privacy/redaction posture for full-body emission is
    // settled).
    attrs.insert("gen_ai.provider.name".into(), kv_str(&row.provider));
    if !row.model.is_empty() {
        attrs.insert("gen_ai.request.model".into(), kv_str(&row.model));
    }
    if let Some(op) = operation_name_from_path(&row.endpoint_path) {
        attrs.insert("gen_ai.operation.name".into(), kv_str(op));
    }
    attrs.insert("gen_ai.request.stream".into(), kv_bool(row.streaming));
    if row.input_tokens > 0 {
        attrs.insert("gen_ai.usage.input_tokens".into(), kv_int(row.input_tokens));
    }
    if row.output_tokens > 0 {
        attrs.insert(
            "gen_ai.usage.output_tokens".into(),
            kv_int(row.output_tokens),
        );
    }
    if let Some(ref s) = row.session_id {
        // The GenAI spec uses gen_ai.conversation.id for "the
        // logical session/thread the request belongs to" — exactly
        // the role our session_id plays in noodle's marking
        // detector (ADR 028).
        attrs.insert("gen_ai.conversation.id".into(), kv_str(s));
    }

    // ── ADR 052 §5 marking ids + frame-tree lineage ──────────────
    // turn_id / role / frame_id / parent_frame_id / depth ride at
    // record scope so each OTLP log + span carries them directly.
    // Mirror the bare attribute names with `gen_ai.*` variants so
    // OTel-spec viewers can group by parent frame or render the
    // frame tree natively. The `gen_ai.*` namespace below is
    // noodle's local extension of the GenAI spec (no published
    // convention exists yet for agent frame lineage).
    if let Some(ref t) = row.turn_id {
        attrs.insert("turn_id".into(), kv_str(t));
        attrs.insert("gen_ai.turn.id".into(), kv_str(t));
    }
    if let Some(ref r) = row.role {
        attrs.insert("role".into(), kv_str(r));
        attrs.insert("gen_ai.frame.role".into(), kv_str(r));
    }
    if let Some(ref f) = row.frame_id {
        attrs.insert("frame_id".into(), kv_str(f));
        attrs.insert("gen_ai.frame.id".into(), kv_str(f));
    }
    if let Some(ref p) = row.parent_frame_id {
        attrs.insert("parent_frame_id".into(), kv_str(p));
        attrs.insert("gen_ai.parent.frame.id".into(), kv_str(p));
    }
    if let Some(d) = row.depth {
        attrs.insert("depth".into(), kv_int(d));
        attrs.insert("gen_ai.frame.depth".into(), kv_int(d));
    }

    // ── ADR 056 context weight ───────────────────────────────────
    // Facts only; cost ratios/dollars are derived by the consumer.
    if let Some(t) = row.context_cache_read_tokens {
        attrs.insert("context.cache_read_tokens".into(), kv_int(t));
        attrs.insert("gen_ai.usage.cache_read_input_tokens".into(), kv_int(t));
    }
    if let Some(t) = row.context_cache_creation_tokens {
        attrs.insert("context.cache_creation_tokens".into(), kv_int(t));
        attrs.insert(
            "gen_ai.usage.cache_creation_input_tokens".into(),
            kv_int(t),
        );
    }
    if let Some(t) = row.context_input_tokens {
        attrs.insert("context.input_tokens".into(), kv_int(t));
    }
    if let Some(b) = row.context_system_bytes {
        attrs.insert("context.system_bytes".into(), kv_int(b));
    }
    if let Some(b) = row.context_tools_bytes {
        attrs.insert("context.tools_bytes".into(), kv_int(b));
    }
    if let Some(c) = row.context_tools_count {
        attrs.insert("context.tools_count".into(), kv_int(c));
    }
    if let Some(b) = row.context_preamble_bytes {
        attrs.insert("context.preamble_bytes".into(), kv_int(b));
    }

    // ── credential identity ──────────────────────────────────────
    if let Some(ref p) = row.api_key_prefix {
        attrs.insert("api_key_prefix".into(), kv_str(p));
    }
    if let Some(ref t) = row.api_key_type {
        attrs.insert("api_key_type".into(), kv_str(t));
    }

    // ── client ───────────────────────────────────────────────────
    if let Some(ref ua) = row.client_user_agent {
        attrs.insert("client_user_agent".into(), kv_str(ua));
    }

    // ── agent (noodle build identity) ────────────────────────────
    attrs.insert("agent.version".into(), kv_str(&row.agent_version));
    attrs.insert("agent.arch".into(), kv_str(&row.agent_arch));

    // ── attribution (context) ────────────────────────────────────
    // The `context_json` column holds the resolved attribution map
    // (slice 042). Promote every entry into a top-level attribute
    // prefixed with `context.` so downstream queries can filter on
    // `context.tool = "Claude Code"`. E4 §B placement strategy
    // says attribution lives at record scope; this is the canonical
    // place.
    if let Some(ref ctx) = row.context_json
        && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(ctx)
    {
        for (k, v) in map {
            let s = v.as_str().map_or_else(|| v.to_string(), str::to_owned);
            // Backward-compat: every entry rides at `context.<k>`.
            attrs.insert(format!("context.{k}"), kv_str(&s));
            // Forward-compat: mirror business-context keys onto the
            // `gen_ai.activity.*` namespace so AI-aware viewers
            // (Phoenix, langsmith, Grafana GenAI) render them natively
            // (ADR 046 §2.3 + ADR 045 §2.6 operator surface). Only
            // declared keys are mirrored — `tool` is the agent
            // identity (attribution), not the activity; line counts
            // arrive in a future slice.
            if let Some(activity_key) = activity_key_for(&k) {
                attrs.insert(format!("gen_ai.activity.{activity_key}"), kv_str(&s));
            }
        }
    }

    // ── provider_metadata as a stringified JSON attribute ────────
    // E4 §B caveat #2: complex nested objects ride as `stringValue`
    // with the JSON serialised; structured `kvlistValue` promotion
    // is deferred to a future slice.
    if let Some(ref pm) = row.provider_metadata_json {
        attrs.insert("provider_metadata.json".into(), kv_str(pm));
    }

    // ── retry telemetry ──────────────────────────────────────────
    if row.retry_count > 0 {
        attrs.insert("retry_count".into(), kv_int(row.retry_count));
    }

    // ── ADR 047 rung 1 brain.* attributes ────────────────────────
    // Each is emitted only when the corresponding column is non-null
    // — back-compat for pre-brain rows (which stay attribute-free).
    if let Some(ref t) = row.brain_thread_id {
        attrs.insert("brain.thread_id".into(), kv_str(t));
    }
    if let Some(i) = row.brain_thread_turn_index {
        attrs.insert("brain.thread_turn_index".into(), kv_int(i));
    }
    if let Some(b) = row.brain_compaction_detected {
        attrs.insert("brain.compaction_detected".into(), kv_bool(b));
    }
    if let Some(b) = row.brain_compaction_directive_present {
        attrs.insert("brain.compaction_directive_present".into(), kv_bool(b));
    }
    if let Some(ref k) = row.brain_compaction_directive_kind {
        attrs.insert("brain.compaction_directive_kind".into(), kv_str(k));
    }
    if let Some(n) = row.brain_blocks_dropped {
        attrs.insert("brain.blocks_dropped".into(), kv_int(n));
    }
    if let Some(n) = row.brain_blocks_added {
        attrs.insert("brain.blocks_added".into(), kv_int(n));
    }
    if let Some(n) = row.brain_estimated_window_tokens {
        attrs.insert("brain.estimated_window_tokens".into(), kv_int(n));
    }
    if let Some(b) = row.brain_api_context_management_beta {
        attrs.insert("brain.api_context_management_beta".into(), kv_bool(b));
    }

    // ── ADR 045 §2.5 policy.* attributes (Watchtower D2) ─────────
    // Same null-tolerant pattern as brain.* — pre-D2 rows stay
    // attribute-free.
    if let Some(ref s) = row.policy_decision {
        attrs.insert("policy.decision".into(), kv_str(s));
    }
    if let Some(ref s) = row.policy_mode {
        attrs.insert("policy.mode".into(), kv_str(s));
    }
    if let Some(r) = row.policy_risk {
        attrs.insert("policy.risk".into(), kv_double(r));
    }
    if let Some(ref s) = row.policy_rule {
        attrs.insert("policy.rule".into(), kv_str(s));
    }
    if let Some(ref s) = row.policy_rationale {
        attrs.insert("policy.rationale".into(), kv_str(s));
    }
    if let Some(ref s) = row.policy_surface {
        attrs.insert("policy.surface".into(), kv_str(s));
    }

    json!({
        "timeUnixNano": row.timestamp.saturating_mul(1_000_000).to_string(),
        "observedTimeUnixNano": row.timestamp.saturating_mul(1_000_000).to_string(),
        "severityNumber": SEVERITY_INFO,
        "severityText": "INFO",
        "body": {
            "stringValue": format!(
                "{} {} {}",
                row.provider,
                row.model,
                row.endpoint_path,
            )
        },
        "attributes": attrs.into_iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>(),
    })
}

/// `OTel` span kind 3 = `CLIENT`. The shipper's perspective on a
/// round-trip is "we (or the proxy on behalf of the agent) called
/// the model" — that's a client span (`OTel` spec).
const SPAN_KIND_CLIENT: i64 = 3;
/// `OTel` span status code 0 = `Unset`, 1 = `Ok`, 2 = `Error`.
const STATUS_CODE_OK: i64 = 1;
const STATUS_CODE_ERROR: i64 = 2;

/// D1.1 — emit the same row as an OTLP Span so off-the-shelf
/// distributed-tracing viewers (Phoenix, Tempo, Jaeger, Honeycomb)
/// render captured sessions natively with no noodle-side UI. The
/// attribute set is identical to [`row_to_otlp_log`] — the span
/// shape is the wire change, not the data.
///
/// Span identity:
///
/// - **`traceId`** (16 bytes) = SHA-256(`session_hash`) prefix; when
///   `session_hash` is missing falls back to SHA-256(`event_id`).
///   Round-trips sharing a session roll up to one trace in the
///   viewer.
/// - **`spanId`** (8 bytes) = SHA-256(`event_id`) prefix. Stable
///   across re-runs over the same `tap.jsonl` — the embellisher's
///   AC #4 idempotency carries through.
/// - **`parentSpanId`** is omitted in v1. Sibling spans under one
///   `traceId` is the simplest correct shape; turn-tree hierarchy
///   (parenting a `tool_use` turn under the `end_turn` that proposed
///   it) is a future slice.
#[must_use]
#[allow(clippy::too_many_lines)] // mirror of row_to_otlp_log; attribute set parity is the point
pub fn row_to_otlp_span(row: &RollupsRow) -> Value {
    let log = row_to_otlp_log(row);
    let attributes = log["attributes"].clone();

    let trace_id = trace_id_for(row);
    let span_id = span_id_for(&row.event_id);
    let start_unix_nano = row.timestamp.saturating_mul(1_000_000);
    let end_unix_nano = start_unix_nano.saturating_add(row.latency_ms.saturating_mul(1_000_000));

    // Span name follows the OTel GenAI semconv recommendation:
    // `<operation> <model>` (e.g. `chat claude-3-5-sonnet`). Falls
    // back to `<provider> <model>` when no operation maps.
    let op = operation_name_from_path(&row.endpoint_path).unwrap_or(row.provider.as_str());
    let name = if row.model.is_empty() {
        op.to_owned()
    } else {
        format!("{op} {}", row.model)
    };

    let (status_code, status_message) = if row.error_type.is_some() || row.status_code >= 400 {
        (
            STATUS_CODE_ERROR,
            row.error_type
                .clone()
                .unwrap_or_else(|| format!("HTTP {}", row.status_code)),
        )
    } else {
        (STATUS_CODE_OK, String::new())
    };

    let mut status = Map::new();
    status.insert("code".into(), json!(status_code));
    if !status_message.is_empty() {
        status.insert("message".into(), json!(status_message));
    }

    json!({
        "traceId": trace_id,
        "spanId": span_id,
        "name": name,
        "kind": SPAN_KIND_CLIENT,
        "startTimeUnixNano": start_unix_nano.to_string(),
        "endTimeUnixNano": end_unix_nano.to_string(),
        "attributes": attributes,
        "status": status,
    })
}

/// SHA-256(`turn_id`) → 16 hex bytes (32 hex chars): **one trace per turn**
/// (ADR 057 — turn = trace). Falls back to `session_hash`, then `event_id`, so
/// every row always has a `traceId` even on legacy/unmarked captures.
fn trace_id_for(row: &RollupsRow) -> String {
    let key = row
        .turn_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| row.session_hash.as_deref().filter(|s| !s.is_empty()))
        .unwrap_or(&row.event_id);
    let digest = Sha256::digest(key.as_bytes());
    hex_lower(&digest[..16])
}

/// SHA-256(`event_id`) → 8 hex bytes (16 hex chars).
fn span_id_for(event_id: &str) -> String {
    let digest = Sha256::digest(event_id.as_bytes());
    hex_lower(&digest[..8])
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Build the per-batch resource-scoped attributes (E4 §B placement
/// strategy: `session_id` + `frame_id` ride at resource scope).
/// Picks the values from the **first** row in the batch; rows in
/// one batch typically share the same session, but consumers can't
/// rely on it — record-scope copies are duplicated above for
/// independent filtering.
#[must_use]
pub fn resource_attributes_for_batch(rows: &[RollupsRow]) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(first) = rows.first() {
        if let Some(ref s) = first.session_id {
            out.push(json!({ "key": "session_id", "value": kv_str(s) }));
        }
        out.push(json!({
            "key": "agent.version",
            "value": kv_str(&first.agent_version),
        }));
        out.push(json!({
            "key": "agent.arch",
            "value": kv_str(&first.agent_arch),
        }));
        out.push(json!({
            "key": "schema_id",
            "value": kv_str(&first.schema_id),
        }));
        out.push(json!({
            "key": "schema_version",
            "value": kv_str(&first.schema_version),
        }));
    }
    out.push(json!({
        "key": "service.name",
        "value": kv_str("noodle-shipper"),
    }));
    out
}

/// Map a `context_json` key (the noodle-native attribution name) to
/// its `gen_ai.activity.*` mirror. Returns `None` for keys that are
/// not part of the activity vocabulary (notably `tool` — the agent
/// identity, attribution-side, not the activity).
fn activity_key_for(key: &str) -> Option<&'static str> {
    match key {
        "work_type" => Some("type"),
        "project" => Some("project"),
        "repo" => Some("repo"),
        "branch" => Some("branch"),
        "issue" => Some("issue"),
        "customer" => Some("customer"),
        _ => None,
    }
}

/// Derive a `gen_ai.operation.name` value from the request's path.
///
/// Returns `None` when the path doesn't match a known generative-AI
/// endpoint family — those rows pass through without the attribute
/// rather than guessing.
///
/// Spec-defined values: `chat`, `text_completion`, `embeddings`,
/// `generate_content`, `retrieval`, `execute_tool`, `create_agent`,
/// `invoke_agent`, `invoke_workflow`.
fn operation_name_from_path(path: &str) -> Option<&'static str> {
    // Anthropic — `/v1/messages` is a chat-style multi-turn endpoint.
    if path == "/v1/messages" || path.starts_with("/v1/messages?") {
        return Some("chat");
    }
    // OpenAI — chat completions + responses are chat-shaped; the
    // legacy completions endpoint is text_completion.
    if path == "/v1/chat/completions" || path == "/v1/responses" {
        return Some("chat");
    }
    if path == "/v1/completions" {
        return Some("text_completion");
    }
    if path == "/v1/embeddings" {
        return Some("embeddings");
    }
    // Google Vertex / Gemini patterns.
    if path.ends_with(":generateContent") || path.ends_with(":streamGenerateContent") {
        return Some("generate_content");
    }
    if path.ends_with(":embedContent") {
        return Some("embeddings");
    }
    None
}

fn kv_str(s: &str) -> Value {
    json!({ "stringValue": s })
}

fn kv_int(n: i64) -> Value {
    json!({ "intValue": n.to_string() })
}

fn kv_bool(b: bool) -> Value {
    json!({ "boolValue": b })
}

fn kv_double(f: f64) -> Value {
    json!({ "doubleValue": f })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> RollupsRow {
        RollupsRow {
            event_id: "nl-7".to_owned(),
            schema_id: "ai-telemetry".to_owned(),
            schema_version: "0.0.2".to_owned(),
            event_type: "api_call".to_owned(),
            timestamp: 1_716_657_600_000,
            provider: "anthropic".to_owned(),
            model: "claude-3-5-sonnet".to_owned(),
            endpoint_path: "/v1/messages".to_owned(),
            streaming: true,
            status_code: 200,
            error_type: None,
            latency_ms: 1500,
            input_tokens: 1234,
            output_tokens: 567,
            api_key_prefix: Some("sk-ant-api03".to_owned()),
            api_key_type: Some("api_key".to_owned()),
            session_id: Some("session-abc".to_owned()),
            session_hash: Some("abc12345".to_owned()),
            client_user_agent: Some("claude-cli/2.1.0".to_owned()),
            agent_version: "0.0.1".to_owned(),
            agent_arch: "aarch64".to_owned(),
            context_json: Some(r#"{"tool":"Claude Code","work_type":"refactor"}"#.to_owned()),
            provider_metadata_json: Some(
                r#"{"provider":"anthropic","request_id":"req_xyz"}"#.to_owned(),
            ),
            correlation_quality: "full".to_owned(),
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

    fn attrs(record: &Value) -> serde_json::Map<String, Value> {
        let arr = record["attributes"].as_array().unwrap();
        arr.iter()
            .map(|kv| (kv["key"].as_str().unwrap().to_owned(), kv["value"].clone()))
            .collect()
    }

    #[test]
    fn correlation_lands_at_record_scope() {
        let row = sample_row();
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        assert_eq!(a["event_id"]["stringValue"], "nl-7");
        assert_eq!(a["session_id"]["stringValue"], "session-abc");
        assert_eq!(a["session_hash"]["stringValue"], "abc12345");
        assert_eq!(a["correlation_quality"]["stringValue"], "full");
    }

    #[test]
    fn usage_token_counts_land_as_int_value() {
        let row = sample_row();
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        // intValue is stringified per OTLP/HTTP JSON spec.
        assert_eq!(a["input_tokens"]["intValue"], "1234");
        assert_eq!(a["output_tokens"]["intValue"], "567");
        assert_eq!(a["status_code"]["intValue"], "200");
        assert_eq!(a["latency_ms"]["intValue"], "1500");
    }

    #[test]
    fn streaming_lands_as_bool_value() {
        let row = sample_row();
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        assert_eq!(a["streaming"]["boolValue"], true);
    }

    #[test]
    fn context_json_promoted_into_prefixed_attributes() {
        let row = sample_row();
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        assert_eq!(a["context.tool"]["stringValue"], "Claude Code");
        assert_eq!(a["context.work_type"]["stringValue"], "refactor");
    }

    #[test]
    fn policy_attrs_emit_when_present() {
        // ADR 045 §2.5 — observe-mode verdict columns map onto
        // policy.* attributes on the OTLP log record.
        let mut row = sample_row();
        row.policy_decision = Some("flag".into());
        row.policy_mode = None; // not an enforcement verb
        row.policy_risk = Some(0.42);
        row.policy_rule = Some("bash.rm_rf".into());
        row.policy_rationale = Some("destructive shell pattern".into());
        row.policy_surface = Some("response.tool_use".into());
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        assert_eq!(a["policy.decision"]["stringValue"], "flag");
        assert!(a.get("policy.mode").is_none(), "mode is optional");
        assert!((a["policy.risk"]["doubleValue"].as_f64().unwrap() - 0.42).abs() < 1e-9);
        assert_eq!(a["policy.rule"]["stringValue"], "bash.rm_rf");
        assert_eq!(
            a["policy.rationale"]["stringValue"],
            "destructive shell pattern"
        );
        assert_eq!(a["policy.surface"]["stringValue"], "response.tool_use");
    }

    #[test]
    fn policy_attrs_absent_when_unset() {
        let row = sample_row();
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        for key in [
            "policy.decision",
            "policy.mode",
            "policy.risk",
            "policy.rule",
            "policy.rationale",
            "policy.surface",
        ] {
            assert!(a.get(key).is_none(), "{key} should be absent");
        }
    }

    #[test]
    fn marking_and_lineage_attrs_emit_with_dual_namespace() {
        // ADR 052 §5: turn_id + role + frame_id + parent_frame_id +
        // depth ride at record scope as both bare names and
        // `gen_ai.*` mirrors so OTel viewers can group / render the
        // frame tree natively.
        let mut row = sample_row();
        row.turn_id = Some("01KTM671B16Y4RMGBFPKGY4TVY".into());
        row.role = Some("sub_agent".into());
        row.frame_id = Some("01KTM671B1FJTW6NGPC5HBHGMH".into());
        row.parent_frame_id = Some("01KTM671B16Y4RMGBFPKGY4TVY".into());
        row.depth = Some(2);
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        // Bare names — backward-compatible queries.
        assert_eq!(a["turn_id"]["stringValue"], "01KTM671B16Y4RMGBFPKGY4TVY");
        assert_eq!(a["role"]["stringValue"], "sub_agent");
        assert_eq!(a["frame_id"]["stringValue"], "01KTM671B1FJTW6NGPC5HBHGMH");
        assert_eq!(
            a["parent_frame_id"]["stringValue"],
            "01KTM671B16Y4RMGBFPKGY4TVY"
        );
        assert_eq!(a["depth"]["intValue"], "2");
        // gen_ai.* mirrors for OTel-aware viewers.
        assert_eq!(
            a["gen_ai.turn.id"]["stringValue"],
            "01KTM671B16Y4RMGBFPKGY4TVY"
        );
        assert_eq!(a["gen_ai.frame.role"]["stringValue"], "sub_agent");
        assert_eq!(
            a["gen_ai.frame.id"]["stringValue"],
            "01KTM671B1FJTW6NGPC5HBHGMH"
        );
        assert_eq!(
            a["gen_ai.parent.frame.id"]["stringValue"],
            "01KTM671B16Y4RMGBFPKGY4TVY"
        );
        assert_eq!(a["gen_ai.frame.depth"]["intValue"], "2");
    }

    #[test]
    fn marking_and_lineage_attrs_absent_when_unset() {
        let row = sample_row(); // None for every lineage field
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        for key in [
            "turn_id",
            "role",
            "frame_id",
            "parent_frame_id",
            "depth",
            "gen_ai.turn.id",
            "gen_ai.frame.role",
            "gen_ai.frame.id",
            "gen_ai.parent.frame.id",
            "gen_ai.frame.depth",
        ] {
            assert!(a.get(key).is_none(), "{key} should be absent");
        }
    }

    #[test]
    fn provider_metadata_rides_as_stringified_json() {
        let row = sample_row();
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        // Stringified JSON; consumers parse it client-side.
        let pm = a["provider_metadata.json"]["stringValue"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(pm).unwrap();
        assert_eq!(parsed["provider"], "anthropic");
        assert_eq!(parsed["request_id"], "req_xyz");
    }

    #[test]
    fn resource_attributes_carry_session_and_agent_identity() {
        let rows = vec![sample_row()];
        let res = resource_attributes_for_batch(&rows);
        let res_attrs: serde_json::Map<String, Value> = res
            .iter()
            .map(|kv| (kv["key"].as_str().unwrap().to_owned(), kv["value"].clone()))
            .collect();
        assert_eq!(res_attrs["session_id"]["stringValue"], "session-abc");
        assert_eq!(res_attrs["agent.version"]["stringValue"], "0.0.1");
        assert_eq!(res_attrs["schema_id"]["stringValue"], "ai-telemetry");
        assert_eq!(res_attrs["service.name"]["stringValue"], "noodle-shipper");
    }

    #[test]
    fn empty_batch_still_emits_service_name() {
        let res = resource_attributes_for_batch(&[]);
        assert_eq!(res.len(), 1);
        assert_eq!(res[0]["key"], "service.name");
    }

    #[test]
    fn gen_ai_attrs_emitted_for_anthropic_chat_with_tokens() {
        let row = sample_row();
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        assert_eq!(a["gen_ai.provider.name"]["stringValue"], "anthropic");
        assert_eq!(
            a["gen_ai.request.model"]["stringValue"],
            "claude-3-5-sonnet"
        );
        assert_eq!(a["gen_ai.operation.name"]["stringValue"], "chat");
        assert_eq!(a["gen_ai.request.stream"]["boolValue"], true);
        assert_eq!(a["gen_ai.usage.input_tokens"]["intValue"], "1234");
        assert_eq!(a["gen_ai.usage.output_tokens"]["intValue"], "567");
        assert_eq!(a["gen_ai.conversation.id"]["stringValue"], "session-abc");
    }

    #[test]
    fn gen_ai_operation_name_recognises_known_paths() {
        let cases = [
            ("/v1/messages", Some("chat")),
            ("/v1/messages?beta=true", Some("chat")),
            ("/v1/chat/completions", Some("chat")),
            ("/v1/responses", Some("chat")),
            ("/v1/completions", Some("text_completion")),
            ("/v1/embeddings", Some("embeddings")),
            (
                "/v1/models/gemini-2.0:generateContent",
                Some("generate_content"),
            ),
            (
                "/v1/models/gemini-2.0:streamGenerateContent",
                Some("generate_content"),
            ),
            ("/v1/models/text-embedding:embedContent", Some("embeddings")),
            ("/healthz", None),
            ("/unknown/path", None),
        ];
        for (path, want) in cases {
            assert_eq!(
                operation_name_from_path(path),
                want,
                "path {path:?} expected {want:?}"
            );
        }
    }

    #[test]
    fn gen_ai_omits_token_counts_when_zero() {
        let mut row = sample_row();
        row.input_tokens = 0;
        row.output_tokens = 0;
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        // Existing non-namespaced ones still emit (0 is meaningful in
        // the legacy attribute set).
        assert_eq!(a["input_tokens"]["intValue"], "0");
        // gen_ai.usage.* omit zero per the spec's "if applicable"
        // recommendation — keeps batches lean for non-completion
        // endpoints like /healthz that share the row shape.
        assert!(!a.contains_key("gen_ai.usage.input_tokens"));
        assert!(!a.contains_key("gen_ai.usage.output_tokens"));
    }

    #[test]
    fn gen_ai_conversation_id_omitted_when_session_absent() {
        let mut row = sample_row();
        row.session_id = None;
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        assert!(!a.contains_key("gen_ai.conversation.id"));
        // gen_ai.provider.name still emits — it's required.
        assert_eq!(a["gen_ai.provider.name"]["stringValue"], "anthropic");
    }

    #[test]
    fn missing_optional_fields_are_omitted() {
        let mut row = sample_row();
        row.session_id = None;
        row.session_hash = None;
        row.context_json = None;
        row.provider_metadata_json = None;
        row.client_user_agent = None;
        row.error_type = None;
        let log = row_to_otlp_log(&row);
        let a = attrs(&log);
        assert!(!a.contains_key("session_id"));
        assert!(!a.contains_key("session_hash"));
        assert!(!a.contains_key("client_user_agent"));
        assert!(!a.contains_key("error_type"));
        assert!(!a.contains_key("provider_metadata.json"));
        // event_id always present.
        assert_eq!(a["event_id"]["stringValue"], "nl-7");
    }
}
