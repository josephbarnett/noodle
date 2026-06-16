//! Public [`Embellisher`] API: read tap.jsonl, pair, map, write.
//!
//! The CLI binary calls into this. The e2e harness also calls into
//! this. Keeping the orchestration here (rather than in `main.rs`)
//! makes it testable end-to-end without spawning a process.
//!
//! Two consumption modes share one pairing core:
//!
//! - **Batch** — [`Embellisher::process_file`] reads `tap.jsonl` to EOF
//!   (via `WireSource::FileRead`) and pairs every request/response by
//!   `event_id`, one row per pair.
//! - **Tail** — the `--watch` driver feeds records one at a time
//!   through [`Embellisher::process_record`] (via
//!   `WireSource::FileTail`); the pairing buffers persist across polls
//!   so a request and its later-arriving response still pair.
//!
//! Both routes call the same `process_record` → `emit_pair`, so "what a
//! pair is" has a single definition.

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;
use tracing::{debug, warn};

use noodle_embellish_core::{Brain, ChainClassifier, PolicyClassifier};

use crate::decoded::decode_pair;
use crate::mapper::{
    enrich_with_brain, enrich_with_context_weight, enrich_with_policy, enrich_with_roundtrip,
    map_decoded_pair,
};
use crate::reader::{
    ReadError, RoundTripView, TapEntryView, read_roundtrips_jsonl, read_tap_jsonl,
};
use crate::sqlite::{SqliteError, SqliteWriter};

#[derive(Debug, Error)]
pub enum EmbellishError {
    #[error(transparent)]
    Read(#[from] ReadError),

    #[error(transparent)]
    Sqlite(#[from] SqliteError),
}

/// Counts surfaced after a batch run. Useful for the CLI summary
/// line and for the e2e test's sanity assertions.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmbellisherStats {
    /// Total tap.jsonl lines parsed.
    pub records_read: usize,
    /// Request records observed.
    pub requests: usize,
    /// Response records observed.
    pub responses: usize,
    /// Pairs that produced a row.
    pub rows_written: usize,
    /// Requests with no matching response (dropped — partial-event
    /// support arrives later per ADR 031 §4.1 / §5.1).
    pub unpaired_requests: usize,
    /// Responses with no matching request (dropped — usually means
    /// the proxy was killed mid-flow or the file was truncated).
    pub orphan_responses: usize,
    /// Slice 042: number of `roundtrips.jsonl` records loaded into
    /// the join index. Zero is normal for pre-040.b captures.
    pub roundtrips_loaded: usize,
    /// Slice 042: number of emitted rows that matched a
    /// `roundtrips.jsonl` record (attribution + correlation data
    /// landed in `context_json`).
    pub rows_with_roundtrip: usize,
}

/// Public orchestration surface.
///
/// Holds the `SQLite` writer plus the request/response pairing buffers
/// and the roundtrips join index. The buffers are fields (not locals)
/// so the same pairing logic serves both modes:
///
/// - **Batch** ([`Self::process_records_with_roundtrips`]) clears the
///   buffers per call — one-shot read-to-EOF, unchanged behaviour.
/// - **Tail** (the `--watch` driver) feeds records one at a time via
///   [`Self::process_record`]; the buffers persist, so a request seen
///   in one poll pairs with its response seen in a later poll.
pub struct Embellisher {
    writer: SqliteWriter,
    pending_requests: HashMap<String, TapEntryView>,
    pending_responses: HashMap<String, TapEntryView>,
    roundtrip_index: HashMap<String, RoundTripView>,
    /// ADR 047 rung 1 — per-process brain observing every decoded
    /// pair. Lives for the lifetime of the embellisher; idle-TTL
    /// thread eviction is a future concern.
    brain: Brain,
    /// ADR 045 §2.2 / §2.4 Watchtower observe-mode classifier. D2.1
    /// ships [`AllowAllClassifier`]; swap in via
    /// [`Self::set_policy_classifier`] once real rules land.
    policy_classifier: Box<dyn PolicyClassifier>,
}

impl Embellisher {
    /// Open the `SQLite` database at `db_path`, creating it (with
    /// schema) if it doesn't exist yet.
    pub fn open(db_path: &Path) -> Result<Self, EmbellishError> {
        Ok(Self::with_writer(SqliteWriter::open(db_path)?))
    }

    /// Open against an in-memory `SQLite` — used by unit tests that
    /// want the full pipeline without a tempfile.
    pub fn open_in_memory() -> Result<Self, EmbellishError> {
        Ok(Self::with_writer(SqliteWriter::open_in_memory()?))
    }

    fn with_writer(writer: SqliteWriter) -> Self {
        Self {
            writer,
            pending_requests: HashMap::new(),
            pending_responses: HashMap::new(),
            roundtrip_index: HashMap::new(),
            brain: Brain::new(),
            // ADR 045 D2.2: production chain — bash destructive
            // rule → allow-all fallback. Callers can override via
            // [`Self::set_policy_classifier`].
            policy_classifier: Box::new(ChainClassifier::d2_default()),
        }
    }

    /// Swap the [`PolicyClassifier`] adapter — the seam D2.2 uses to
    /// install a real rules classifier without touching the
    /// embellisher's pair loop.
    pub fn set_policy_classifier(&mut self, classifier: Box<dyn PolicyClassifier>) {
        self.policy_classifier = classifier;
    }

    /// Replace the roundtrips join index. The batch path sets this from
    /// the records it loads; the `--watch` driver refreshes it as
    /// `roundtrips.jsonl` grows so late-arriving attribution can enrich
    /// pairs that have not yet been emitted.
    pub fn set_roundtrip_index(&mut self, index: HashMap<String, RoundTripView>) {
        self.roundtrip_index = index;
    }

    /// Pair a single record against the persistent buffers, emitting a
    /// row the moment its partner is present. This is the streaming
    /// entry point (the `--watch` driver calls it per tailed record);
    /// the batch path routes through it too, so both modes share one
    /// definition of "what a pair is and when it emits".
    ///
    /// Idempotent at the DB layer: `emit_pair` uses the `event_id` PK
    /// with `INSERT OR IGNORE`, so a replay (e.g. a restarted tail that
    /// re-reads from offset 0) never double-writes.
    pub fn process_record(
        &mut self,
        record: TapEntryView,
        stats: &mut EmbellisherStats,
    ) -> Result<(), EmbellishError> {
        stats.records_read += 1;
        let Some(event_id) = record.event_id().map(str::to_owned) else {
            warn!("skipping tap record with no event_id");
            return Ok(());
        };
        if record.is_request() {
            stats.requests += 1;
            if let Some(resp) = self.pending_responses.remove(&event_id) {
                self.emit_pair(record, resp, stats)?;
            } else {
                self.pending_requests.entry(event_id).or_insert(record);
            }
        } else if record.is_response() {
            stats.responses += 1;
            if let Some(req) = self.pending_requests.remove(&event_id) {
                self.emit_pair(req, record, stats)?;
            } else {
                self.pending_responses.entry(event_id).or_insert(record);
            }
        } else {
            debug!(direction = ?record.direction(), "ignoring tap record with unknown direction");
        }
        Ok(())
    }

    /// Process every record in `tap_path` in one batch. Pairs are
    /// keyed by `event_id`; the first request and the first response
    /// sharing an `event_id` form a pair, and any subsequent records
    /// with the same id are logged at debug and ignored. Returns
    /// counts useful for verification.
    ///
    /// Slice 042: `roundtrips.jsonl` (sibling file in the same
    /// directory) is loaded into a per-`event_id` index and joined
    /// onto every emitted row. The attribution map promotes into
    /// `context_json`; `correlation_quality` upgrades from
    /// `wire_only` to `full`. A missing roundtrips file is
    /// tolerated — pre-040.b captures see zero joined rows.
    pub fn process_file(&mut self, tap_path: &Path) -> Result<EmbellisherStats, EmbellishError> {
        let records = read_tap_jsonl(tap_path)?;
        // Look for roundtrips.jsonl next to tap.jsonl (the default
        // layout — same `~/.noodle/` directory). When tap_path is
        // not a default name (test fixture), accept the file living
        // alongside it.
        let roundtrips_path = tap_path.parent().map_or_else(
            || Path::new("roundtrips.jsonl").to_path_buf(),
            |p| p.join("roundtrips.jsonl"),
        );
        let roundtrips = read_roundtrips_jsonl(&roundtrips_path)?;
        self.process_records_with_roundtrips(records, roundtrips)
    }

    /// Process an already-parsed batch. Same semantics as
    /// [`Self::process_file`]; the split lets tests pass synthetic
    /// records without writing them to disk first.
    pub fn process_records(
        &mut self,
        records: Vec<TapEntryView>,
    ) -> Result<EmbellisherStats, EmbellishError> {
        self.process_records_with_roundtrips(records, Vec::new())
    }

    /// Slice 042: process a batch of tap records joined against
    /// pre-loaded `roundtrips.jsonl` entries. The roundtrip index
    /// is keyed by `event_id`; entries with no `event_id` are
    /// silently ignored.
    pub fn process_records_with_roundtrips(
        &mut self,
        records: Vec<TapEntryView>,
        roundtrips: Vec<RoundTripView>,
    ) -> Result<EmbellisherStats, EmbellishError> {
        // Batch semantics: each call is a fresh one-shot pass, so reset
        // the persistent buffers first. (The `--watch` driver, by
        // contrast, calls `process_record` directly and lets them
        // accumulate across polls.)
        self.pending_requests.clear();
        self.pending_responses.clear();

        let roundtrip_index: HashMap<String, RoundTripView> = roundtrips
            .into_iter()
            .filter_map(|rt| rt.event_id().map(str::to_owned).map(|id| (id, rt)))
            .collect();

        let mut stats = EmbellisherStats {
            roundtrips_loaded: roundtrip_index.len(),
            ..Default::default()
        };
        self.set_roundtrip_index(roundtrip_index);

        for record in records {
            self.process_record(record, &mut stats)?;
        }

        stats.unpaired_requests = self.pending_requests.len();
        stats.orphan_responses = self.pending_responses.len();
        Ok(stats)
    }

    /// Route a paired request/response through the decoder → mapper →
    /// writer pipeline. The reader → decoder → mapper pipeline
    /// replaces the reader → inline-parse → mapper pipeline that
    /// shipped in S16 (refactor slice S23). The on-disk `SQLite` row
    /// is byte-identical because the mapping logic is the same; only
    /// the input plumbing changed.
    fn emit_pair(
        &mut self,
        request: TapEntryView,
        response: TapEntryView,
        stats: &mut EmbellisherStats,
    ) -> Result<(), EmbellishError> {
        // Slice 042 AC #4 idempotency: reuse the proxy's per-flow
        // `event_id` as the row PK. The proxy stamps it on every
        // `tap.jsonl` record; re-runs over the same file produce
        // identical PKs and the INSERT OR IGNORE in SqliteWriter
        // de-duplicates on the DB side.
        let event_id_pk = request.event_id().map(str::to_owned);
        let pair = decode_pair(request, response);
        // ADR 047 rung 1: observe every paired round-trip. The brain
        // owns its per-thread state; observe() returns the
        // observation we stamp onto the row alongside roundtrip
        // enrichment.
        let brain_obs = self.brain.observe(&pair);
        // ADR 045 §2.5 Watchtower observe-mode: classify every pair,
        // stamp the verdict onto the row's policy.* columns. The
        // classifier is pure — no I/O, no mutation.
        let policy_obs = self.policy_classifier.classify(&pair);
        if let Some(base_row) = map_decoded_pair(&pair) {
            // Look the roundtrip up from the persistent index. The
            // borrow of `roundtrip_index` ends at `enrich_with_roundtrip`
            // (NLL), before `self.writer` is touched.
            let roundtrip = event_id_pk
                .as_deref()
                .and_then(|id| self.roundtrip_index.get(id));
            let has_roundtrip = roundtrip.is_some();
            let mut row = enrich_with_roundtrip(base_row, roundtrip);
            row = enrich_with_brain(row, brain_obs);
            row = enrich_with_policy(row, policy_obs);
            // ADR 056 — measure context weight off the same decoded pair
            // and stamp it. Pure; None when no usage block.
            row = enrich_with_context_weight(
                row,
                noodle_embellish_core::measure_context_weight(&pair),
            );
            if let Some(id) = event_id_pk {
                row.event_id = id;
            }
            self.writer.insert(row)?;
            stats.rows_written += 1;
            if has_roundtrip {
                stats.rows_with_roundtrip += 1;
            }
        }
        Ok(())
    }

    /// Return the underlying writer (for tests + the CLI's verify
    /// path).
    #[must_use]
    pub fn writer(&self) -> &SqliteWriter {
        &self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(event_id: &str) -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "request",
            "timestamp": "2026-05-25T17:00:00.000Z",
            "event_id": event_id,
            "provider": "anthropic",
            "method": "POST",
            "url": "https://api.anthropic.com/v1/messages",
            "headers": { "User-Agent": ["test"] },
            "body": { "model": "claude-3-5-sonnet" }
        }))
    }

    fn resp(event_id: &str, in_tok: u64, out_tok: u64) -> TapEntryView {
        TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-05-25T17:00:01.000Z",
            "event_id": event_id,
            "provider": "anthropic",
            "status": 200,
            "headers": { "Content-Type": ["application/json"] },
            "usage": {
                "tokens": { "input_tokens": in_tok, "output_tokens": out_tok }
            }
        }))
    }

    fn resp_with_bash(event_id: &str, command: &str) -> TapEntryView {
        // Response shape the AnthropicDecoder consumes — see
        // `noodle_domain::decoders::anthropic` tests for the canonical
        // `content.blocks[].kind = "tool_use"` example.
        TapEntryView::from_value(json!({
            "direction": "response",
            "timestamp": "2026-05-25T17:00:01.000Z",
            "event_id": event_id,
            "provider": "anthropic",
            "status": 200,
            "headers": { "Content-Type": ["application/json"] },
            "content": {
                "blocks": [
                    {
                        "kind": "tool_use",
                        "tool_use_id": "toolu_abc",
                        "tool_name": "Bash",
                        "input": { "command": command }
                    }
                ]
            },
            "usage": {
                "tokens": { "input_tokens": 10, "output_tokens": 20 }
            }
        }))
    }

    fn read_policy_row(e: &Embellisher, event_id: &str) -> (String, String, f64) {
        let mut stmt = e
            .writer()
            .conn()
            .prepare(
                "SELECT policy_decision, policy_rule, policy_risk
                 FROM ai_telemetry_v_0_0_2 WHERE event_id = ?1",
            )
            .unwrap();
        let mut rows = stmt
            .query_map([event_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })
            .unwrap();
        rows.next().unwrap().unwrap()
    }

    #[test]
    fn d2_chain_flags_bash_destructive_pair_end_to_end() {
        // The seam: synthetic Bash tool_use with `rm -rf /tmp/x` →
        // AnthropicDecoder → BashDestructiveClassifier → SQLite row
        // carries `policy_decision='flag'`, `policy_rule='bash.rm_rf'`.
        // Replaces a synthetic-only port test — this exercises the
        // full embellisher path the production binary runs.
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e
            .process_records(vec![req("e1"), resp_with_bash("e1", "rm -rf /tmp/x")])
            .unwrap();
        assert_eq!(stats.rows_written, 1);
        let (decision, rule, risk) = read_policy_row(&e, "e1");
        assert_eq!(decision, "flag");
        assert_eq!(rule, "bash.rm_rf");
        assert!(risk > 0.5, "risk should be non-trivial, got {risk}");
    }

    #[test]
    fn d2_chain_allows_safe_bash_pair_end_to_end() {
        // Safe Bash → bash classifier returns None → chain falls
        // through to AllowAllClassifier → SQLite row carries
        // `policy_decision='allow'`, `policy_rule='default'`.
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e
            .process_records(vec![req("e2"), resp_with_bash("e2", "ls -la /tmp")])
            .unwrap();
        assert_eq!(stats.rows_written, 1);
        let (decision, rule, _risk) = read_policy_row(&e, "e2");
        assert_eq!(decision, "allow");
        assert_eq!(rule, "default");
    }

    #[test]
    fn pairs_request_and_response_then_writes_row() {
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e
            .process_records(vec![req("a"), resp("a", 10, 20)])
            .unwrap();
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.responses, 1);
        assert_eq!(stats.rows_written, 1);
        assert_eq!(stats.unpaired_requests, 0);
        assert_eq!(stats.orphan_responses, 0);
        assert_eq!(e.writer().row_count().unwrap(), 1);
    }

    #[test]
    fn handles_response_arriving_before_request() {
        // The pair buffer must tolerate either order — in batch
        // mode the file is sequential but for robustness we don't
        // assume request precedes response.
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e
            .process_records(vec![resp("a", 10, 20), req("a")])
            .unwrap();
        assert_eq!(stats.rows_written, 1);
    }

    #[test]
    fn process_record_pairs_across_calls_with_persistent_buffers() {
        // Tail-mode behaviour: a request observed in one poll must pair
        // with its response observed in a *later* poll. The batch path's
        // per-call buffers wouldn't survive that; `process_record` uses
        // the Embellisher's persistent buffers.
        let mut e = Embellisher::open_in_memory().unwrap();
        let mut stats = EmbellisherStats::default();

        e.process_record(req("a"), &mut stats).unwrap();
        assert_eq!(stats.rows_written, 0, "no row until the response arrives");

        e.process_record(resp("a", 10, 20), &mut stats).unwrap();
        assert_eq!(
            stats.rows_written, 1,
            "the later response completes the pair"
        );
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.responses, 1);
        assert_eq!(e.writer().row_count().unwrap(), 1);
    }

    #[test]
    fn unpaired_request_counts_but_writes_no_row() {
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e.process_records(vec![req("a")]).unwrap();
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.rows_written, 0);
        assert_eq!(stats.unpaired_requests, 1);
    }

    #[test]
    fn orphan_response_counts_but_writes_no_row() {
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e.process_records(vec![resp("a", 1, 2)]).unwrap();
        assert_eq!(stats.responses, 1);
        assert_eq!(stats.rows_written, 0);
        assert_eq!(stats.orphan_responses, 1);
    }

    // ─── Slice 042 acceptance criteria ──────────────────────────

    fn rt(event_id: &str, tool: &str, work_type: &str) -> RoundTripView {
        RoundTripView::from_value(json!({
            "kind": "round_trip",
            "event_id": event_id,
            "session_id": format!("session-{event_id}"),
            "turn_id": format!("turn-{event_id}"),
            "frame_id": format!("frame-{event_id}"),
            "flow_id": 0,
            "started_at_unix_ms": 1_716_657_600_000_i64,
            "completed_at_unix_ms": 1_716_657_601_000_i64,
            "duration_ms": 1000,
            "request": {
                "host": "api.anthropic.com",
                "endpoint": "/v1/messages",
                "method": "POST",
                "directive_enhanced": false,
                "tools_resolved": []
            },
            "attributions": {
                "tool": tool,
                "work_type": work_type
            },
            "evidence": {"hints":[], "artifacts":[], "audits":[]}
        }))
    }

    /// AC #1 + #5: a roundtrip join populates `context_json` (the
    /// attribution map) and stamps `correlation_quality = full`.
    #[test]
    fn roundtrip_join_lands_attributions_in_context_and_upgrades_quality() {
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e
            .process_records_with_roundtrips(
                vec![req("a"), resp("a", 10, 20)],
                vec![rt("a", "Claude Code", "refactor")],
            )
            .unwrap();
        assert_eq!(stats.rows_written, 1);
        assert_eq!(stats.rows_with_roundtrip, 1);
        assert_eq!(stats.roundtrips_loaded, 1);

        let conn = e.writer().conn();
        let (context_json, quality): (String, String) = conn
            .query_row(
                "SELECT context_json, correlation_quality FROM ai_telemetry_v_0_0_2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(context_json.contains("Claude Code"));
        assert!(context_json.contains("refactor"));
        assert_eq!(quality, "full");
    }

    /// AC #4 idempotency: re-running over the same input does not
    /// duplicate rows.
    #[test]
    fn rerunning_same_records_is_idempotent_on_event_id() {
        let mut e = Embellisher::open_in_memory().unwrap();
        let records = vec![req("a"), resp("a", 10, 20), req("b"), resp("b", 1, 2)];

        e.process_records_with_roundtrips(records.clone(), vec![])
            .unwrap();
        assert_eq!(e.writer().row_count().unwrap(), 2);

        // Second run over the same records — same event_ids, so the
        // INSERT OR IGNORE de-duplicates.
        e.process_records_with_roundtrips(records, vec![]).unwrap();
        assert_eq!(
            e.writer().row_count().unwrap(),
            2,
            "re-running must not duplicate rows"
        );
    }

    /// AC #5 variant: no roundtrip join → `wire_only` when marks are
    /// present, `minimal` otherwise.
    #[test]
    fn no_roundtrip_join_yields_wire_only_when_marks_present() {
        // request carries no marks block in this synthetic fixture
        // (the helper above doesn't add one), so we expect minimal.
        let mut e = Embellisher::open_in_memory().unwrap();
        e.process_records_with_roundtrips(
            vec![req("a"), resp("a", 1, 2)],
            vec![], // no roundtrip
        )
        .unwrap();
        let quality: String = e
            .writer()
            .conn()
            .query_row(
                "SELECT correlation_quality FROM ai_telemetry_v_0_0_2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            quality == "minimal" || quality == "wire_only",
            "expected minimal or wire_only without roundtrip, got {quality}"
        );
    }

    /// PK contract: the row's `event_id` matches the `tap.jsonl`
    /// `event_id`, not a freshly minted ULID.
    #[test]
    fn row_event_id_uses_tap_jsonl_event_id() {
        let mut e = Embellisher::open_in_memory().unwrap();
        e.process_records_with_roundtrips(vec![req("nl-42"), resp("nl-42", 1, 2)], vec![])
            .unwrap();
        let id: String = e
            .writer()
            .conn()
            .query_row("SELECT event_id FROM ai_telemetry_v_0_0_2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(id, "nl-42");
    }

    #[test]
    fn multiple_pairs_all_produce_rows() {
        let mut e = Embellisher::open_in_memory().unwrap();
        let stats = e
            .process_records(vec![
                req("a"),
                resp("a", 10, 20),
                req("b"),
                resp("b", 30, 40),
                req("c"),
                resp("c", 50, 60),
            ])
            .unwrap();
        assert_eq!(stats.rows_written, 3);
        assert_eq!(e.writer().row_count().unwrap(), 3);
    }
}
