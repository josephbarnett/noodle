//! `SideEffectSink` adapters (ADR 020 §2.1).
//!
//! Four driven adapters:
//! - [`TracingSink`] — emits one `tracing` event per `SideEffect`.
//! - [`InMemorySink`] — `Arc<Mutex<Vec<SideEffect>>>` for tests.
//! - [`MultiSideEffectSink`] — fan-out composite.
//! - [`SideEffectsJsonlSink`] — file-backed JSONL writer; one
//!   line per emission.
//!
//! Carved out of `noodle-adapters` per ADR 039 §4 — these adapters
//! are file-/runtime-coupled and proxy-host-only. The
//! [`SideEffectSink`][noodle_core::layered::SideEffectSink] port
//! itself stays in `noodle-core`.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use noodle_core::layered::{ResolvedRecord, SideEffect, SideEffectSink};
use serde::Serialize;

// ─── TracingSink ──────────────────────────────────────────────

/// Emits one `tracing` event per `SideEffect`. Default-on
/// adapter for operators who already capture noodle's `tracing`
/// output; gives the §16 empty-on-error contract observability
/// "for free" without any I/O setup.
///
/// Levels:
/// - `Hint` / `Artifact` / `Resolved` → `INFO`
/// - `Audit { kind: Errored | InvariantViolation, .. }` → `WARN`
/// - other `Audit` kinds (e.g. `Enhanced`, `Redacted`,
///   `Filtered`) → `DEBUG` (chatty; the operator opts in via
///   the env filter).
pub struct TracingSink;

impl Default for TracingSink {
    fn default() -> Self {
        Self
    }
}

impl SideEffectSink for TracingSink {
    fn record(&self, effect: SideEffect) {
        use noodle_core::layered::AuditKind;
        match effect {
            SideEffect::Hint(h) => {
                tracing::info!(
                    target: "noodle::side_effect",
                    category = %h.category,
                    value = %h.value,
                    confidence = h.confidence,
                    source = %h.source,
                    "hint emitted"
                );
            }
            SideEffect::Artifact(a) => {
                tracing::info!(
                    target: "noodle::side_effect",
                    name = %a.name,
                    value = %a.value,
                    transform = %a.source_transform,
                    flow_id = a.flow_id,
                    "artifact captured"
                );
            }
            SideEffect::Audit(a) => match a.kind {
                AuditKind::Errored | AuditKind::InvariantViolation | AuditKind::MintFailed => {
                    tracing::warn!(
                        target: "noodle::side_effect",
                        kind = ?a.kind,
                        transform = %a.transform,
                        flow_id = a.flow_id,
                        detail = %a.detail,
                        "audit (failure)"
                    );
                }
                AuditKind::LeafMinted => {
                    tracing::info!(
                        target: "noodle::side_effect",
                        kind = ?a.kind,
                        transform = %a.transform,
                        flow_id = a.flow_id,
                        detail = %a.detail,
                        "audit (leaf minted)"
                    );
                }
                _ => {
                    tracing::debug!(
                        target: "noodle::side_effect",
                        kind = ?a.kind,
                        transform = %a.transform,
                        flow_id = a.flow_id,
                        detail = %a.detail,
                        "audit"
                    );
                }
            },
            SideEffect::Resolved(r) => {
                tracing::info!(
                    target: "noodle::side_effect",
                    session = %r.session.prefix(),
                    flow_id = r.flow_id,
                    categories = r.resolved.len(),
                    "resolved attribution record"
                );
            }
        }
    }
}

// ─── InMemorySink ──────────────────────────────────────────────

/// Records every `SideEffect` into an in-memory `Vec`. For
/// tests. Cheap to construct, cheap to assert against.
///
/// `lock()`-poisoning is treated as a test bug (panics); this
/// type is **not** meant for production. Production sinks like
/// `TracingSink` recover gracefully; `InMemorySink` should fail
/// loudly so a panicked test thread is visible immediately.
#[derive(Default)]
pub struct InMemorySink {
    inner: Mutex<Vec<SideEffect>>,
}

impl InMemorySink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the currently-recorded effects. Returns a clone
    /// so the caller can inspect without holding the lock.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex has been poisoned (a writer
    /// thread panicked while holding the lock). This sink is for
    /// tests; poisoning is a test bug and should surface loudly.
    #[must_use]
    pub fn snapshot(&self) -> Vec<SideEffect> {
        self.inner
            .lock()
            .expect("InMemorySink mutex poisoned")
            .clone()
    }

    /// Number of effects recorded so far.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex has been poisoned. Same
    /// rationale as `snapshot`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("InMemorySink mutex poisoned")
            .len()
    }

    /// True when no effects have been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Helper: return only the `Resolved` records, in emission
    /// order. Used by tests that want to assert on the final
    /// attribution output without filtering the full vec.
    #[must_use]
    pub fn resolved_records(&self) -> Vec<ResolvedRecord> {
        self.snapshot()
            .into_iter()
            .filter_map(|e| match e {
                SideEffect::Resolved(r) => Some(r),
                _ => None,
            })
            .collect()
    }
}

impl SideEffectSink for InMemorySink {
    fn record(&self, effect: SideEffect) {
        self.inner
            .lock()
            .expect("InMemorySink mutex poisoned")
            .push(effect);
    }
}

// ─── MultiSideEffectSink ──────────────────────────────────────────────

/// Fan-out composite: records to every wrapped sink in
/// registration order. One child's panic during `record` does
/// not stop subsequent children — each child call is wrapped in
/// `catch_unwind` so the bus continues. Mirrors the failure-
/// isolation contract called out in ADR 020 §2.1.
///
/// Composition lives here, not in the engine — operators wrap
/// multiple sinks into a single `Arc<dyn SideEffectSink>` slot
/// when wiring the engine builder.
pub struct MultiSideEffectSink {
    sinks: Vec<Arc<dyn SideEffectSink>>,
}

impl MultiSideEffectSink {
    #[must_use]
    pub fn new(sinks: Vec<Arc<dyn SideEffectSink>>) -> Self {
        Self { sinks }
    }
}

impl SideEffectSink for MultiSideEffectSink {
    fn record(&self, effect: SideEffect) {
        for sink in &self.sinks {
            // catch_unwind isolates a panicking child sink so the
            // others still receive the effect. The cost is a small
            // boundary per call; the alternative (one panic poisons
            // the bus for the rest of the flow) is unacceptable for
            // an observability path that is meant to be non-fatal.
            let sink = Arc::clone(sink);
            let effect_clone = effect.clone();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                sink.record(effect_clone);
            }));
            if result.is_err() {
                tracing::warn!(
                    target: "noodle::side_effect",
                    "MultiSideEffectSink: a child sink panicked; \
                     continuing with remaining sinks"
                );
            }
        }
    }
}

// ─── SideEffectsJsonlSink ──────────────────────────────────────

/// Wire shape for the file-backed sink. One JSONL line per
/// `SideEffect`. Discriminated by `kind`; payload fields per
/// kind. Keeping the wire shape explicit (not the auto-derived
/// `serde_json` of the `SideEffect` enum) so the file is a
/// stable, parseable contract that downstream tools (the viewer,
/// dashboards, ad-hoc `jq`) can rely on.
/// ADR 023 §2.3 correlation block, serialised additively on every
/// `JsonlEntry` variant. Fields are flattened onto the entry so
/// consumers see a stable top-level shape — `event_id` /
/// `turn_id` / `session_id` / `frame_id` (ADR 052 §5) — without an
/// extra nesting level.
///
/// `at_unix_ms` is **not** part of this struct because every
/// variant already carries a timestamp slot in its native shape
/// (`Audit::at_unix_ms`, `Artifact::captured_at_unix_ms`,
/// `Resolved::at_unix_ms`). The engine drain stamps that legacy
/// slot from the correlation (via
/// [`SideEffect::stamp_correlation`]) so consumers see a single
/// canonical `at_unix_ms` on disk — no duplicate JSON keys. The
/// `Hint` variant is the exception: it has no native timestamp,
/// so the JSONL emits a dedicated top-level `at_unix_ms` field
/// sourced from `correlation.at_unix_ms`.
///
/// `None` values are skipped — a record produced outside an
/// inspectable flow (e.g. cert-mint audit) omits the four
/// optional keys entirely.
#[derive(Serialize)]
#[allow(clippy::struct_field_names)] // all four are correlation *ids* by design
struct CorrelationFields<'a> {
    event_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    /// ADR 052 §5: the frame-tree node id (the spawning `tool_use.id`)
    /// replaces the retired `agent_run_id` on the correlation block.
    /// Sourced from `Correlation::agent_run_id`, which the proxy
    /// already stamps from `marks.frame_id` (wirelog.rs §5).
    #[serde(skip_serializing_if = "Option::is_none")]
    frame_id: Option<&'a str>,
}

impl<'a> CorrelationFields<'a> {
    fn from(c: &'a noodle_core::layered::Correlation) -> Self {
        Self {
            event_id: c.event_id.as_str(),
            turn_id: c.turn_id.as_ref().map(smol_str::SmolStr::as_str),
            session_id: c.session_id.as_ref().map(smol_str::SmolStr::as_str),
            frame_id: c.agent_run_id.as_ref().map(smol_str::SmolStr::as_str),
        }
    }
}

/// Serde `skip_serializing_if` predicate. `serde` requires the
/// predicate to take `&T`, so the clippy ref-on-small-type lint
/// is intentional here — passing by value would not satisfy the
/// derive macro's signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JsonlEntry<'a> {
    Hint {
        category: &'a str,
        value: &'a str,
        confidence: f32,
        source: &'a str,
        /// Drain-time wall-clock stamp. Sourced from
        /// `Hint.correlation.at_unix_ms` because `Hint` itself
        /// carries no native timestamp; absent (0) for unstamped
        /// effects.
        #[serde(skip_serializing_if = "is_zero_u64")]
        at_unix_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none", flatten)]
        correlation: Option<CorrelationFields<'a>>,
    },
    Artifact {
        name: &'a str,
        value: &'a str,
        source_transform: &'a str,
        flow_id: u64,
        captured_at_unix_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none", flatten)]
        correlation: Option<CorrelationFields<'a>>,
    },
    Audit {
        kind_inner: &'a str,
        transform: &'a str,
        flow_id: u64,
        at_unix_ms: u64,
        detail: &'a serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none", flatten)]
        correlation: Option<CorrelationFields<'a>>,
    },
    Resolved {
        session_prefix: String,
        flow_id: u64,
        at_unix_ms: u64,
        resolved: std::collections::BTreeMap<String, String>,
        /// Full ADR 028 `MarkingSessionId` value when the response
        /// flow carried one (040.a AC #2). Distinct from
        /// `session_prefix` (hash-derived from request headers).
        /// Lives outside `correlation` so the existing
        /// `session_prefix` key keeps its meaning.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none", flatten)]
        correlation: Option<CorrelationFields<'a>>,
    },
}

/// File-backed `SideEffectSink` that writes one JSONL line per
/// emission (ADR 020 §5.1).
///
/// `Mutex` guards a `BufWriter<File>` — `record` acquires the
/// lock, writes the line + newline, and returns. Writes are
/// **synchronous**; the non-blocking contract from
/// Drop-on-full backpressure: when the channel is saturated (slow disk,
/// big artifact burst), `record()` increments a counter and returns —
/// the engine hot path never blocks on file I/O. Errors during write
/// are logged via `tracing` and discarded.
///
/// Call [`Self::shutdown`] at graceful-shutdown time to drain the
/// writer task; on plain `Drop` the task aborts (any unflushed bytes
/// are lost — same `DoS` posture as ADR 020 §6 specifies).
pub struct SideEffectsJsonlSink {
    path: PathBuf,
    /// Held inside an `Option` so [`Self::shutdown`] can drop the
    /// canonical sender and let the writer task observe channel
    /// close. `record()` reads via `Mutex::lock` (uncontended for
    /// the single-writer, many-emitter pattern attribution uses)
    /// and clones the inner `Sender` once per call before issuing
    /// the non-blocking `try_send`.
    tx: std::sync::Mutex<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>,
    dropped: std::sync::atomic::AtomicU64,
    join: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

const SIDE_EFFECT_CHANNEL_CAPACITY: usize = 1024;
const SIDE_EFFECT_DROP_LOG_PERIOD: u64 = 64;
const SIDE_EFFECT_FLUSH_BATCH: usize = 64;
const SIDE_EFFECT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

impl SideEffectsJsonlSink {
    /// Open (truncating) `path`, spawn the writer task on the current
    /// tokio runtime, and return a sink ready to be wrapped in `Arc`.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if the file cannot be created
    /// or opened for writing.
    pub async fn spawn(path: &Path) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(SIDE_EFFECT_CHANNEL_CAPACITY);
        let path_owned = path.to_path_buf();
        let join = tokio::spawn(run_writer(file, rx, path_owned.clone()));
        Ok(Self {
            path: path_owned,
            tx: std::sync::Mutex::new(Some(tx)),
            dropped: std::sync::atomic::AtomicU64::new(0),
            join: std::sync::Mutex::new(Some(join)),
        })
    }

    /// Synchronous create — opens the file (and spawns the writer) on
    /// the current tokio runtime. Convenience for non-async callers
    /// like `tap_setup::install` that already hold a handle.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if the file cannot be created.
    ///
    /// # Panics
    ///
    /// Panics if called outside a tokio runtime.
    pub fn create(path: &Path) -> std::io::Result<Self> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(Self::spawn(path))
        })
    }

    /// Path the sink writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of records dropped due to channel saturation.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Graceful drain. Drops the canonical sender (releasing the
    /// writer task from its `recv` loop), awaits the writer's final
    /// flush + file close. Subsequent `record()` calls fall through
    /// to the dropped-count counter as no-ops. Idempotent.
    ///
    /// # Panics
    ///
    /// Panics if the join-handle mutex or sender mutex is poisoned
    /// (would mean another thread previously panicked while holding
    /// it; the sink is unusable at that point).
    pub async fn shutdown(&self) {
        // Drop the canonical sender so the writer task's
        // `rx.recv() -> None` branch fires, draining pending bytes
        // and closing the file.
        let _ = self.tx.lock().expect("poisoned").take();
        let join = self.join.lock().expect("poisoned").take();
        if let Some(j) = join {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), j).await;
        }
    }
}

impl SideEffectSink for SideEffectsJsonlSink {
    fn record(&self, effect: SideEffect) {
        let entry = match &effect {
            SideEffect::Hint(h) => JsonlEntry::Hint {
                category: h.category.as_str(),
                value: h.value.as_str(),
                confidence: h.confidence,
                source: h.source.as_str(),
                at_unix_ms: h.correlation.as_ref().map_or(0, |c| c.at_unix_ms),
                correlation: h.correlation.as_ref().map(CorrelationFields::from),
            },
            SideEffect::Artifact(a) => JsonlEntry::Artifact {
                name: a.name.as_str(),
                value: a.value.as_str(),
                source_transform: a.source_transform.as_str(),
                flow_id: a.flow_id,
                captured_at_unix_ms: a.captured_at_unix_ms,
                correlation: a.correlation.as_ref().map(CorrelationFields::from),
            },
            SideEffect::Audit(a) => JsonlEntry::Audit {
                kind_inner: audit_kind_str(a.kind),
                transform: a.transform.as_str(),
                flow_id: a.flow_id,
                at_unix_ms: a.at_unix_ms,
                detail: &a.detail,
                correlation: a.correlation.as_ref().map(CorrelationFields::from),
            },
            SideEffect::Resolved(r) => JsonlEntry::Resolved {
                session_prefix: r.session.prefix().to_string(),
                flow_id: r.flow_id,
                at_unix_ms: r.at_unix_ms,
                resolved: r
                    .resolved
                    .0
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                session_id: r
                    .correlation
                    .as_ref()
                    .and_then(|c| c.session_id.as_ref())
                    .map(smol_str::SmolStr::as_str),
                correlation: r.correlation.as_ref().map(CorrelationFields::from),
            },
        };
        let mut line = match serde_json::to_vec(&entry) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "noodle::side_effect",
                    error = %e,
                    "SideEffectsJsonlSink: failed to serialise; dropping"
                );
                return;
            }
        };
        line.push(b'\n');
        let tx = {
            let Ok(guard) = self.tx.lock() else {
                tracing::warn!(
                    target: "noodle::side_effect",
                    "SideEffectsJsonlSink: tx mutex poisoned; dropping"
                );
                return;
            };
            // Post-shutdown sinks have `None` here; drop silently
            // (the dropped-count counter only tracks pre-shutdown
            // saturation, not legitimate post-drain emissions).
            let Some(t) = guard.as_ref() else { return };
            t.clone()
        };
        if tx.try_send(line).is_err() {
            let n = self
                .dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if n.is_multiple_of(SIDE_EFFECT_DROP_LOG_PERIOD) {
                tracing::warn!(
                    target: "noodle::side_effect",
                    dropped_total = n,
                    "SideEffectsJsonlSink: writer channel saturated, dropping events"
                );
            }
        }
    }
}

async fn run_writer(
    file: tokio::fs::File,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    path: PathBuf,
) {
    use tokio::io::AsyncWriteExt;
    let mut writer = tokio::io::BufWriter::new(file);
    let mut pending: usize = 0;
    let start = tokio::time::Instant::now() + SIDE_EFFECT_FLUSH_INTERVAL;
    let mut tick = tokio::time::interval_at(start, SIDE_EFFECT_FLUSH_INTERVAL);

    loop {
        tokio::select! {
            biased;
            line = rx.recv() => {
                let Some(line) = line else { break };
                if let Err(e) = writer.write_all(&line).await {
                    tracing::warn!(
                        target: "noodle::side_effect",
                        ?e,
                        path = %path.display(),
                        "SideEffectsJsonlSink writer: write_all failed"
                    );
                    continue;
                }
                pending += 1;
                if pending >= SIDE_EFFECT_FLUSH_BATCH {
                    side_effect_flush(&mut writer, &path).await;
                    pending = 0;
                }
            }
            _ = tick.tick() => {
                if pending > 0 {
                    side_effect_flush(&mut writer, &path).await;
                    pending = 0;
                }
            }
        }
    }

    // Channel closed: drain anything still buffered.
    while let Ok(line) = rx.try_recv() {
        let _ = writer.write_all(&line).await;
    }
    side_effect_flush(&mut writer, &path).await;
    if let Err(e) = writer.shutdown().await {
        tracing::warn!(
            target: "noodle::side_effect",
            ?e,
            path = %path.display(),
            "SideEffectsJsonlSink writer: shutdown failed"
        );
    }
}

async fn side_effect_flush(writer: &mut tokio::io::BufWriter<tokio::fs::File>, path: &Path) {
    use tokio::io::AsyncWriteExt;
    if let Err(e) = writer.flush().await {
        tracing::warn!(
            target: "noodle::side_effect",
            ?e,
            path = %path.display(),
            "SideEffectsJsonlSink writer: flush failed"
        );
    }
}

fn audit_kind_str(kind: noodle_core::layered::AuditKind) -> &'static str {
    use noodle_core::layered::AuditKind;
    match kind {
        AuditKind::Enhanced => "enhanced",
        AuditKind::Redacted => "redacted",
        AuditKind::Filtered => "filtered",
        AuditKind::Errored => "errored",
        AuditKind::InvariantViolation => "invariant_violation",
        AuditKind::LeafMinted => "leaf_minted",
        AuditKind::MintFailed => "mint_failed",
    }
}

// ─── RoundTripSink (ADR 023 §2.2 / story 040.b) ────────────────────

/// `Clock` DI seam for `RoundTripSink` — substitutes a `FakeClock`
/// in unit tests where the assembled record's
/// `completed_at_unix_ms` must be deterministic. The production
/// impl reads system time.
pub trait Clock: Send + Sync + 'static {
    fn now_unix_ms(&self) -> u64;
}

/// Default production clock — `SystemTime::now()` against the
/// UNIX epoch. Falls back to 0 on a clock that pre-dates the
/// epoch (impossible in practice; defensive against system
/// misconfiguration rather than panicking).
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

const ROUND_TRIP_CHANNEL_CAPACITY: usize = 1024;
const ROUND_TRIP_DROP_LOG_PERIOD: u64 = 64;
const ROUND_TRIP_FLUSH_BATCH: usize = 64;
const ROUND_TRIP_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);
/// Max concurrent in-flight flow buffers. When this many flows
/// are pending (request seen but response `WireEvent` not yet
/// arrived), new flow opens evict the oldest buffer with a
/// dropped-flow counter increment. Sized to comfortably cover the
/// dozens-of-concurrent-flows worst case of a single Claude Code
/// session running tools in parallel.
const ROUND_TRIP_MAX_PENDING_FLOWS: usize = 1024;
/// Per-flow cap on collected evidence (Hint + Artifact + Audit
/// total). Pathological cases (a transform emitting hundreds of
/// audits per round-trip) drop further evidence with a counter
/// increment rather than ballooning per-flow memory. ADR 023 §2.2
/// admits "small constant per flow".
const ROUND_TRIP_MAX_EVIDENCE_PER_FLOW: usize = 256;

/// Per-flow accumulator. Updated by both the `WireSink::record`
/// (request + response metadata) and the `SideEffectSink::record`
/// (evidence + correlation + attributions) paths. Emitted when
/// the response `WireEvent` arrives — that is the deterministic
/// flow-finish signal in the wire-log layer's emission order
/// (drain fires before the response `emit()` call in
/// `noodle_proxy::wirelog::TeeBody::poll_frame`, so by the time
/// the response `WireEvent` lands every drained side-effect has
/// already been buffered here).
#[derive(Debug, Default)]
struct FlowBuffer {
    request: Option<noodle_core::layered::RoundTripRequest>,
    response: Option<noodle_core::layered::RoundTripResponse>,
    started_at_unix_ms: Option<u64>,
    completed_at_unix_ms: Option<u64>,
    correlation: Option<noodle_core::layered::Correlation>,
    flow_id: noodle_core::layered::FlowId,
    attributions: noodle_core::Resolved,
    usage: Option<serde_json::Value>,
    hints: Vec<noodle_core::layered::Hint>,
    artifacts: Vec<noodle_core::layered::Artifact>,
    audits: Vec<noodle_core::layered::AuditEvent>,
    /// Count of evidence records dropped because the per-flow cap
    /// was hit. Surfaced into the `audits` field as a synthetic
    /// `Filtered` audit on emit so downstream consumers can detect
    /// truncation.
    dropped_evidence: u64,
    /// Number of `Resolved` `SideEffect`s observed for this flow.
    /// The wirelog drains the request side ONCE (`request_outbound`)
    /// and the response side ONCE (`EngineState::finish`, SSE-only).
    /// `>= 2` is the gate for "the engine inspected the response"
    /// — non-LLM HTTP exchanges max out at 1.
    resolved_count: u32,
}

impl FlowBuffer {
    fn record_hint(&mut self, h: noodle_core::layered::Hint) {
        if self.total_evidence() >= ROUND_TRIP_MAX_EVIDENCE_PER_FLOW {
            self.dropped_evidence += 1;
            return;
        }
        self.hints.push(h);
    }
    fn record_artifact(&mut self, a: noodle_core::layered::Artifact) {
        if self.total_evidence() >= ROUND_TRIP_MAX_EVIDENCE_PER_FLOW {
            self.dropped_evidence += 1;
            return;
        }
        self.artifacts.push(a);
    }
    fn record_audit(&mut self, a: noodle_core::layered::AuditEvent) {
        if self.total_evidence() >= ROUND_TRIP_MAX_EVIDENCE_PER_FLOW {
            self.dropped_evidence += 1;
            return;
        }
        self.audits.push(a);
    }
    fn total_evidence(&self) -> usize {
        self.hints.len() + self.artifacts.len() + self.audits.len()
    }
}

/// File-backed sink that assembles one [`RoundTripRecord`] per
/// completed HTTP round-trip and writes one JSONL line to
/// `roundtrips.jsonl` (ADR 023 §2.1 / §2.2).
///
/// Implements **both** [`SideEffectSink`] and
/// [`noodle_core::WireSink`]. The sink reads:
///
/// - request + response metadata from the wire-log layer's
///   `WireEvent` stream (`WireSink::record`).
/// - hints / artifacts / audits / Resolved attributions from the
///   engine drain's `SideEffect` stream (`SideEffectSink::record`).
///
/// Buffers are keyed by `event_id` (the proxy-minted
/// `request_id`, equal to ADR 023's `correlation.event_id`).
/// Emission happens when the response `WireEvent` for a buffered
/// flow arrives — the deterministic flow-finish signal.
/// Buffers whose flow ends without a response (transport error,
/// upstream timeout) hang until the LRU cap evicts them; the
/// dropped-flow counter increments. AC #6 / AC #7 satisfied.
pub struct RoundTripSink {
    path: PathBuf,
    clock: Arc<dyn Clock>,
    /// Pending per-flow buffers indexed by `event_id`. Behind a
    /// `Mutex` — record contention is on the order of dozens of
    /// concurrent flows, all locks are short (vec push + map
    /// lookup). LRU eviction order is preserved by a side-Vec.
    buffers: Mutex<RoundTripBuffers>,
    tx: Mutex<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>,
    dropped_lines: std::sync::atomic::AtomicU64,
    dropped_flows: std::sync::atomic::AtomicU64,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Internal buffer table — held inside the `Mutex`. The LRU
/// order vector is the canonical insertion order; the map is
/// the keyed lookup.
#[derive(Debug, Default)]
struct RoundTripBuffers {
    map: std::collections::HashMap<smol_str::SmolStr, FlowBuffer>,
    /// Insertion order for LRU eviction at cap. `map.len()`
    /// equals `order.len()`.
    order: std::collections::VecDeque<smol_str::SmolStr>,
}

impl RoundTripBuffers {
    fn entry_mut(&mut self, event_id: &smol_str::SmolStr) -> &mut FlowBuffer {
        if !self.map.contains_key(event_id) {
            // New buffer — evict oldest if at cap.
            if self.map.len() >= ROUND_TRIP_MAX_PENDING_FLOWS
                && let Some(oldest) = self.order.pop_front()
            {
                self.map.remove(&oldest);
            }
            self.map.insert(event_id.clone(), FlowBuffer::default());
            self.order.push_back(event_id.clone());
        }
        self.map.get_mut(event_id).expect("just inserted")
    }

    fn remove(&mut self, event_id: &smol_str::SmolStr) -> Option<FlowBuffer> {
        let v = self.map.remove(event_id)?;
        self.order.retain(|k| k != event_id);
        Some(v)
    }
}

impl RoundTripSink {
    /// Open (truncating) `path`, spawn the writer task on the
    /// current tokio runtime, and return a sink ready to wrap in
    /// `Arc`.
    ///
    /// # Errors
    ///
    /// Returns the underlying I/O error if the file cannot be
    /// created or opened for writing.
    pub async fn spawn(path: &Path, clock: Arc<dyn Clock>) -> std::io::Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .await?;
        let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(ROUND_TRIP_CHANNEL_CAPACITY);
        let path_owned = path.to_path_buf();
        let join = tokio::spawn(run_round_trip_writer(file, rx, path_owned.clone()));
        Ok(Self {
            path: path_owned,
            clock,
            buffers: Mutex::new(RoundTripBuffers::default()),
            tx: Mutex::new(Some(tx)),
            dropped_lines: std::sync::atomic::AtomicU64::new(0),
            dropped_flows: std::sync::atomic::AtomicU64::new(0),
            join: Mutex::new(Some(join)),
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Number of round-trip JSONL lines dropped because the
    /// writer channel was saturated.
    #[must_use]
    pub fn dropped_lines(&self) -> u64 {
        self.dropped_lines
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Number of flows evicted from the pending-buffer cap before
    /// a response `WireEvent` arrived for them.
    #[must_use]
    pub fn dropped_flows(&self) -> u64 {
        self.dropped_flows
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Graceful drain. Drops the canonical sender so the writer
    /// task observes channel close, then awaits its final flush.
    ///
    /// # Panics
    ///
    /// Panics if the mutex is poisoned (another thread panicked
    /// while holding it; the sink is unusable at that point).
    pub async fn shutdown(&self) {
        let _ = self.tx.lock().expect("poisoned").take();
        let join = self.join.lock().expect("poisoned").take();
        if let Some(j) = join {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), j).await;
        }
    }

    /// Internal: serialize an assembled `RoundTripRecord` and
    /// hand the line off to the writer task. Drop-on-full posture
    /// — the engine path is never blocked on file I/O.
    fn write_line(&self, record: &noodle_core::layered::RoundTripRecord) {
        let entry = round_trip_entry(record);
        let mut line = match serde_json::to_vec(&entry) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "noodle::round_trip",
                    error = %e,
                    "RoundTripSink: serialise failed; dropping"
                );
                return;
            }
        };
        line.push(b'\n');
        let tx = {
            let Ok(guard) = self.tx.lock() else {
                tracing::warn!(
                    target: "noodle::round_trip",
                    "RoundTripSink: tx mutex poisoned; dropping"
                );
                return;
            };
            match guard.as_ref() {
                Some(t) => t.clone(),
                None => return, // post-shutdown; silently drop
            }
        };
        if tx.try_send(line).is_err() {
            let prev = self
                .dropped_lines
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if (prev + 1).is_multiple_of(ROUND_TRIP_DROP_LOG_PERIOD) {
                tracing::warn!(
                    target: "noodle::round_trip",
                    dropped_total = prev + 1,
                    "RoundTripSink: channel saturated; dropping lines (logged every \
                     {ROUND_TRIP_DROP_LOG_PERIOD} drops)"
                );
            }
        }
    }

    /// Assemble + emit when the response wire event lands for a
    /// pending flow. Same code path also fires when the request
    /// side terminates without a response (transport error /
    /// timeout) but a `complete: false` response was synthesized
    /// — both routes converge here.
    ///
    /// **Filter:** only round-trips the engine actually inspected
    /// land in `roundtrips.jsonl`. Non-LLM HTTP exchanges
    /// (bootstrap calls, `/v1/mcp/...`, account settings, etc.)
    /// may still produce a request-side `Resolved` (the
    /// `UserAgentDetector` fires on any matched request codec)
    /// but the engine never inspects the response. The signal:
    /// **two or more `Resolved`s for the same `event_id`** — the
    /// first from `request_outbound`'s drain, the second from
    /// `EngineState::finish`'s drain (only fires for SSE on a
    /// matched cell). Single-Resolved buffers are non-inspectable
    /// transit and excluded, matching ADR 023 §1's "LLM round
    /// trip" framing and AC #4's count-equals-/v1/messages
    /// assertion.
    fn try_emit_for(&self, event_id: &smol_str::SmolStr) {
        let buf = {
            let Ok(mut guard) = self.buffers.lock() else {
                tracing::warn!(
                    target: "noodle::round_trip",
                    "RoundTripSink: buffer mutex poisoned"
                );
                return;
            };
            let ready = matches!(
                guard.map.get(event_id),
                Some(b) if b.request.is_some()
                    && b.response.is_some()
                    && b.resolved_count >= 2
            );
            if !ready {
                return;
            }
            guard.remove(event_id)
        };
        let Some(buf) = buf else { return };

        let correlation = buf.correlation.unwrap_or_else(|| {
            // Defensive: response WireEvent arrived but the drain
            // never fired (engine declined the flow). Build a
            // minimal correlation from what we have so the record
            // still emits.
            noodle_core::layered::Correlation {
                event_id: event_id.clone(),
                turn_id: None,
                session_id: None,
                agent_run_id: None,
                at_unix_ms: buf
                    .completed_at_unix_ms
                    .unwrap_or_else(|| self.clock.now_unix_ms()),
            }
        });
        let started_at_unix_ms = buf.started_at_unix_ms.unwrap_or(correlation.at_unix_ms);
        let completed_at_unix_ms = buf
            .completed_at_unix_ms
            .unwrap_or_else(|| self.clock.now_unix_ms());

        let mut audits = buf.audits;
        if buf.dropped_evidence > 0 {
            audits.push(noodle_core::layered::AuditEvent {
                kind: noodle_core::layered::AuditKind::Filtered,
                layer: noodle_core::layered::Layer::VendorSemantics,
                transform: smol_str::SmolStr::new_static("round_trip_sink"),
                flow_id: buf.flow_id,
                at_unix_ms: completed_at_unix_ms,
                detail: serde_json::json!({
                    "reason": "per_flow_evidence_cap_reached",
                    "dropped": buf.dropped_evidence,
                    "cap": ROUND_TRIP_MAX_EVIDENCE_PER_FLOW,
                }),
                correlation: Some(correlation.clone()),
            });
        }

        let record = noodle_core::layered::RoundTripRecord {
            correlation,
            flow_id: buf.flow_id,
            started_at_unix_ms,
            completed_at_unix_ms,
            request: buf.request.unwrap_or_else(default_round_trip_request),
            response: buf.response,
            attributions: buf.attributions,
            usage: buf.usage,
            evidence: noodle_core::layered::RoundTripEvidence {
                hints: buf.hints,
                artifacts: buf.artifacts,
                audits,
            },
        };
        self.write_line(&record);
    }
}

fn default_round_trip_request() -> noodle_core::layered::RoundTripRequest {
    noodle_core::layered::RoundTripRequest {
        host: smol_str::SmolStr::new_static(""),
        endpoint: smol_str::SmolStr::new_static(""),
        method: smol_str::SmolStr::new_static(""),
        user_agent: None,
        model: None,
        directive_enhanced: false,
        tools_resolved: Vec::new(),
    }
}

impl SideEffectSink for RoundTripSink {
    fn record(&self, effect: SideEffect) {
        let event_id = match effect.correlation() {
            Some(c) => c.event_id.clone(),
            None => {
                // Effects bypassing the engine drain (cert-mint
                // audits with no inspection flow) carry no
                // correlation. They are not part of any round-trip
                // and do not belong in `roundtrips.jsonl`.
                return;
            }
        };
        if event_id.is_empty() {
            return;
        }
        let Ok(mut guard) = self.buffers.lock() else {
            return;
        };
        let buf = guard.entry_mut(&event_id);
        match effect {
            SideEffect::Hint(h) => buf.record_hint(h),
            SideEffect::Artifact(a) => buf.record_artifact(a),
            SideEffect::Audit(a) => buf.record_audit(a),
            SideEffect::Resolved(r) => {
                // Resolved is the engine's per-flow attribution
                // emission. Merge into the buffer; emission is
                // gated on (a) the response WireEvent landing
                // (the deterministic flow-finish signal) AND
                // (b) `resolved_count >= 2` (proving the engine
                // actually inspected the response — see
                // `try_emit_for`'s filter doc).
                buf.flow_id = r.flow_id;
                buf.resolved_count = buf.resolved_count.saturating_add(1);
                if let Some(c) = r.correlation.clone() {
                    buf.correlation = Some(c);
                }
                for (k, v) in r.resolved.0 {
                    buf.attributions.0.insert(k, v);
                }
            }
        }
    }
}

impl noodle_core::WireSink for RoundTripSink {
    fn record(&self, event: noodle_core::WireEvent) {
        // ADR 030 §4.3 patch events do not start or end a flow;
        // ignore them. The `record_patch` path on `WireSink`
        // already defaults to no-op so we don't need to override.
        let event_id = event.request_id.clone();
        match event.direction {
            noodle_core::WireDirection::Request => {
                let request = round_trip_request_from(&event);
                let started_at = event.ts_unix_ms;
                let Ok(mut guard) = self.buffers.lock() else {
                    return;
                };
                let buf = guard.entry_mut(&event_id);
                buf.request = Some(request);
                buf.started_at_unix_ms = Some(started_at);
            }
            noodle_core::WireDirection::Response => {
                let response = round_trip_response_from(&event);
                let usage = event.usage.as_ref().map(round_trip_usage_json);
                let completed_at = event.ts_unix_ms;
                {
                    let Ok(mut guard) = self.buffers.lock() else {
                        return;
                    };
                    let buf = guard.entry_mut(&event_id);
                    buf.response = Some(response);
                    buf.completed_at_unix_ms = Some(completed_at);
                    if usage.is_some() {
                        buf.usage = usage;
                    }
                }
                // Now emit if request side has landed.
                self.try_emit_for(&event_id);
            }
        }
    }
}

fn round_trip_request_from(
    event: &noodle_core::WireEvent,
) -> noodle_core::layered::RoundTripRequest {
    let host = host_from_url(event.url.as_deref()).unwrap_or_default();
    let endpoint = path_from_url(event.url.as_deref()).unwrap_or_default();
    let method = event
        .method
        .clone()
        .unwrap_or_else(|| smol_str::SmolStr::new_static(""));
    let user_agent = event
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("user-agent"))
        .map(|h| smol_str::SmolStr::from(h.value.as_str()));
    let model = extract_model_from_body(&event.body_in);
    // `body_in` ≠ `body_out` is exactly the AttributionEnhancer
    // signal — when the enhancer ran it appended bytes to the
    // request system block, so the lengths differ. Codecs that
    // don't enhance leave the bytes byte-faithful (ADR 018 §8).
    let directive_enhanced =
        !event.body_in.is_empty() && !event.body_out.is_empty() && event.body_in != event.body_out;
    let tools_resolved = extract_tool_results(&event.body_in);
    noodle_core::layered::RoundTripRequest {
        host,
        endpoint,
        method,
        user_agent,
        model,
        directive_enhanced,
        tools_resolved,
    }
}

fn round_trip_response_from(
    event: &noodle_core::WireEvent,
) -> noodle_core::layered::RoundTripResponse {
    let status = event.status.unwrap_or(0);
    let kind = response_kind_from_headers(&event.headers);
    // v1 assumes complete-on-arrival; ADR 023 §4 calls out the
    // `complete: false` shape for SSE-error / body-truncation
    // paths but the wire-log layer does not yet surface that
    // signal. Wire it through later.
    let complete = true;
    let stop_reason = extract_stop_reason(&event.body_in);
    let tools_invoked = extract_tools_invoked(&event.body_in);
    noodle_core::layered::RoundTripResponse {
        status,
        kind,
        complete,
        stop_reason,
        tools_invoked,
    }
}

fn response_kind_from_headers(headers: &[noodle_core::HeaderPair]) -> smol_str::SmolStr {
    let ct = headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("content-type"))
        .map(|h| h.value.to_ascii_lowercase());
    match ct.as_deref() {
        Some(c) if c.starts_with("text/event-stream") => smol_str::SmolStr::new_static("sse"),
        Some(c) if c.starts_with("application/json") => smol_str::SmolStr::new_static("json"),
        _ => smol_str::SmolStr::new_static("other"),
    }
}

fn host_from_url(url: Option<&str>) -> Option<smol_str::SmolStr> {
    let url = url?;
    // Cheap parse — full URI machinery is overkill here. We want
    // the authority between `://` and the next `/`.
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = after_scheme.split('/').next()?;
    if host.is_empty() {
        None
    } else {
        Some(smol_str::SmolStr::from(host))
    }
}

fn path_from_url(url: Option<&str>) -> Option<smol_str::SmolStr> {
    let url = url?;
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let mut parts = after_scheme.splitn(2, '/');
    let _authority = parts.next();
    let rest = parts.next().unwrap_or("");
    // Strip query string.
    let path = rest.split('?').next().unwrap_or("");
    let mut full = String::with_capacity(path.len() + 1);
    full.push('/');
    full.push_str(path);
    Some(smol_str::SmolStr::from(full))
}

fn extract_model_from_body(body: &bytes::Bytes) -> Option<smol_str::SmolStr> {
    if body.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")
        .and_then(serde_json::Value::as_str)
        .map(smol_str::SmolStr::from)
}

fn extract_stop_reason(body: &bytes::Bytes) -> Option<smol_str::SmolStr> {
    if body.is_empty() {
        return None;
    }
    // Anthropic SSE: scan for the first
    // `"stop_reason":"<val>"` occurrence — `message_delta` emits
    // exactly one. Same shortcut the marking detector uses.
    let s = std::str::from_utf8(body).ok()?;
    let needle = "\"stop_reason\":\"";
    let i = s.find(needle)?;
    let rest = &s[i + needle.len()..];
    let end = rest.find('"')?;
    Some(smol_str::SmolStr::from(&rest[..end]))
}

fn extract_tool_results(body: &bytes::Bytes) -> Vec<noodle_core::layered::ToolResolution> {
    if body.is_empty() {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let messages = v.get("messages").and_then(serde_json::Value::as_array);
    let Some(messages) = messages else {
        return Vec::new();
    };
    // Last user message is where `tool_result` blocks ride.
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(serde_json::Value::as_str) == Some("user"));
    let Some(msg) = last_user else {
        return Vec::new();
    };
    let content = msg.get("content").and_then(serde_json::Value::as_array);
    let Some(content) = content else {
        return Vec::new();
    };
    content
        .iter()
        .filter_map(|c| {
            if c.get("type").and_then(serde_json::Value::as_str) != Some("tool_result") {
                return None;
            }
            let tool_use_id = c.get("tool_use_id").and_then(serde_json::Value::as_str)?;
            let name = c
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let is_error = c
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            Some(noodle_core::layered::ToolResolution {
                tool_use_id: smol_str::SmolStr::from(tool_use_id),
                name: smol_str::SmolStr::from(name),
                is_error,
            })
        })
        .collect()
}

fn extract_tools_invoked(body: &bytes::Bytes) -> Vec<noodle_core::layered::ToolInvocation> {
    if body.is_empty() {
        return Vec::new();
    }
    // Response side: Anthropic SSE carries `content_block_start`
    // events whose block is `{"type":"tool_use","id":"…","name":"…"}`.
    // Cheap scan rather than full SSE parse — the wire-log layer's
    // assembled `content_blocks` block isn't on `WireEvent`
    // directly; we walk the body verbatim.
    let Ok(s) = std::str::from_utf8(body) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("\"type\":\"tool_use\"") {
        let after = &rest[i..];
        let Some(id) = scan_quoted_field(after, "\"id\":\"") else {
            break;
        };
        let name = scan_quoted_field(after, "\"name\":\"").unwrap_or_default();
        out.push(noodle_core::layered::ToolInvocation { id, name });
        rest = &after[16..];
    }
    out
}

fn scan_quoted_field(s: &str, needle: &str) -> Option<smol_str::SmolStr> {
    let i = s.find(needle)?;
    let rest = &s[i + needle.len()..];
    let end = rest.find('"')?;
    Some(smol_str::SmolStr::from(&rest[..end]))
}

/// JSONL entry shape for one `RoundTripRecord` line. Pins the
/// ADR 023 §4 wire format. Lives alongside the type's
/// rust-native form because `noodle-core` does not depend on
/// `serde` for `RoundTripRecord` — keep the wire shape in the
/// crate that owns the file boundary.
#[derive(Serialize)]
struct RoundTripEntry<'a> {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    /// ADR 052 §5: frame-tree node id (the spawning `tool_use.id`),
    /// sourced from `Correlation::agent_run_id` (proxy-stamped from
    /// `marks.frame_id`). Replaces the retired `agent_run_id` key the
    /// embellisher's `RoundTripView::frame_id()` now reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    frame_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_id: Option<&'a str>,
    event_id: &'a str,
    flow_id: u64,
    started_at_unix_ms: u64,
    completed_at_unix_ms: u64,
    duration_ms: u64,
    request: RoundTripRequestEntry<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<RoundTripResponseEntry<'a>>,
    attributions: std::collections::BTreeMap<&'a str, &'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<&'a serde_json::Value>,
    evidence: RoundTripEvidenceEntry<'a>,
}

#[derive(Serialize)]
struct RoundTripRequestEntry<'a> {
    host: &'a str,
    endpoint: &'a str,
    method: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_agent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    directive_enhanced: bool,
    tools_resolved: Vec<ToolResolutionEntry<'a>>,
}

#[derive(Serialize)]
struct ToolResolutionEntry<'a> {
    tool_use_id: &'a str,
    name: &'a str,
    is_error: bool,
}

#[derive(Serialize)]
struct RoundTripResponseEntry<'a> {
    status: u16,
    kind: &'a str,
    complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<&'a str>,
    tools_invoked: Vec<ToolInvocationEntry<'a>>,
}

#[derive(Serialize)]
struct ToolInvocationEntry<'a> {
    id: &'a str,
    name: &'a str,
}

#[derive(Serialize)]
struct RoundTripEvidenceEntry<'a> {
    hints: Vec<HintEntry<'a>>,
    artifacts: Vec<ArtifactEntry<'a>>,
    audits: Vec<AuditEntry<'a>>,
}

#[derive(Serialize)]
struct HintEntry<'a> {
    category: &'a str,
    value: &'a str,
    confidence: f32,
    source: &'a str,
}

#[derive(Serialize)]
struct ArtifactEntry<'a> {
    name: &'a str,
    value: &'a str,
    source_transform: &'a str,
    flow_id: u64,
    captured_at_unix_ms: u64,
}

#[derive(Serialize)]
struct AuditEntry<'a> {
    kind_inner: &'a str,
    transform: &'a str,
    flow_id: u64,
    at_unix_ms: u64,
    detail: &'a serde_json::Value,
}

fn round_trip_entry(r: &noodle_core::layered::RoundTripRecord) -> RoundTripEntry<'_> {
    RoundTripEntry {
        kind: "round_trip",
        session_id: r
            .correlation
            .session_id
            .as_ref()
            .map(smol_str::SmolStr::as_str),
        frame_id: r
            .correlation
            .agent_run_id
            .as_ref()
            .map(smol_str::SmolStr::as_str),
        turn_id: r
            .correlation
            .turn_id
            .as_ref()
            .map(smol_str::SmolStr::as_str),
        event_id: r.correlation.event_id.as_str(),
        flow_id: r.flow_id,
        started_at_unix_ms: r.started_at_unix_ms,
        completed_at_unix_ms: r.completed_at_unix_ms,
        duration_ms: r.duration_ms(),
        request: RoundTripRequestEntry {
            host: r.request.host.as_str(),
            endpoint: r.request.endpoint.as_str(),
            method: r.request.method.as_str(),
            user_agent: r.request.user_agent.as_ref().map(smol_str::SmolStr::as_str),
            model: r.request.model.as_ref().map(smol_str::SmolStr::as_str),
            directive_enhanced: r.request.directive_enhanced,
            tools_resolved: r
                .request
                .tools_resolved
                .iter()
                .map(|t| ToolResolutionEntry {
                    tool_use_id: t.tool_use_id.as_str(),
                    name: t.name.as_str(),
                    is_error: t.is_error,
                })
                .collect(),
        },
        response: r.response.as_ref().map(|resp| RoundTripResponseEntry {
            status: resp.status,
            kind: resp.kind.as_str(),
            complete: resp.complete,
            stop_reason: resp.stop_reason.as_ref().map(smol_str::SmolStr::as_str),
            tools_invoked: resp
                .tools_invoked
                .iter()
                .map(|t| ToolInvocationEntry {
                    id: t.id.as_str(),
                    name: t.name.as_str(),
                })
                .collect(),
        }),
        attributions: r
            .attributions
            .0
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect(),
        usage: r.usage.as_ref(),
        evidence: RoundTripEvidenceEntry {
            hints: r
                .evidence
                .hints
                .iter()
                .map(|h| HintEntry {
                    category: h.category.as_str(),
                    value: h.value.as_str(),
                    confidence: h.confidence,
                    source: h.source.as_str(),
                })
                .collect(),
            artifacts: r
                .evidence
                .artifacts
                .iter()
                .map(|a| ArtifactEntry {
                    name: a.name.as_str(),
                    value: a.value.as_str(),
                    source_transform: a.source_transform.as_str(),
                    flow_id: a.flow_id,
                    captured_at_unix_ms: a.captured_at_unix_ms,
                })
                .collect(),
            audits: r
                .evidence
                .audits
                .iter()
                .map(|a| AuditEntry {
                    kind_inner: audit_kind_str(a.kind),
                    transform: a.transform.as_str(),
                    flow_id: a.flow_id,
                    at_unix_ms: a.at_unix_ms,
                    detail: &a.detail,
                })
                .collect(),
        },
    }
}

fn round_trip_usage_json(usage: &noodle_core::WireUsage) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    if let Some(tokens) = &usage.tokens {
        let mut t = serde_json::Map::new();
        t.insert("input_tokens".into(), tokens.input.into());
        t.insert("output_tokens".into(), tokens.output.into());
        if let Some(v) = tokens.cached_read {
            t.insert("cache_read_tokens".into(), v.into());
        }
        if let Some(v) = tokens.cached_creation {
            t.insert("cache_write_tokens".into(), v.into());
        }
        if let Some(v) = tokens.reasoning {
            t.insert("reasoning_tokens".into(), v.into());
        }
        // Story 040.b AC #8: nested per-TTL cache-creation
        // breakdown surfaces in the same shape the ai-telemetry
        // v0.0.2 schema expects (sibling keys inside
        // `cache_creation`).
        if let Some(cc) = &tokens.cache_creation {
            let mut cc_obj = serde_json::Map::new();
            if let Some(v) = cc.ephemeral_5m_input_tokens {
                cc_obj.insert("ephemeral_5m_input_tokens".into(), v.into());
            }
            if let Some(v) = cc.ephemeral_1h_input_tokens {
                cc_obj.insert("ephemeral_1h_input_tokens".into(), v.into());
            }
            t.insert("cache_creation".into(), serde_json::Value::Object(cc_obj));
        }
        if !tokens.vendor_extras.is_empty() {
            let extras_obj: serde_json::Map<String, serde_json::Value> = tokens
                .vendor_extras
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            t.insert(
                "vendor_extras".into(),
                serde_json::Value::Object(extras_obj),
            );
        }
        o.insert("tokens".into(), serde_json::Value::Object(t));
    }
    if let Some(latency) = usage.latency
        && (latency.time_to_first_byte_ms.is_some() || latency.total_ms.is_some())
    {
        let mut l = serde_json::Map::new();
        if let Some(v) = latency.time_to_first_byte_ms {
            l.insert("time_to_first_byte_ms".into(), v.into());
        }
        if let Some(v) = latency.total_ms {
            l.insert("total_ms".into(), v.into());
        }
        o.insert("latency".into(), serde_json::Value::Object(l));
    }
    // Story 040.b AC #8: siblings of `tokens` per the
    // ai-telemetry v0.0.2 schema.
    if let Some(st) = &usage.service_tier {
        o.insert(
            "service_tier".into(),
            serde_json::Value::String(st.as_str().to_owned()),
        );
    }
    if let Some(ig) = &usage.inference_geo {
        o.insert(
            "inference_geo".into(),
            serde_json::Value::String(ig.as_str().to_owned()),
        );
    }
    serde_json::Value::Object(o)
}

async fn run_round_trip_writer(
    file: tokio::fs::File,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
    path: PathBuf,
) {
    let mut writer = tokio::io::BufWriter::with_capacity(64 * 1024, file);
    let mut batched: usize = 0;
    let mut interval = tokio::time::interval(ROUND_TRIP_FLUSH_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            msg = rx.recv() => {
                use tokio::io::AsyncWriteExt;
                let Some(bytes) = msg else {
                    let _ = writer.flush().await;
                    return;
                };
                if let Err(e) = writer.write_all(&bytes).await {
                    tracing::warn!(
                        target: "noodle::round_trip",
                        error = %e,
                        path = %path.display(),
                        "RoundTripSink: write_all failed"
                    );
                }
                batched += 1;
                if batched >= ROUND_TRIP_FLUSH_BATCH {
                    if let Err(e) = writer.flush().await {
                        tracing::warn!(
                            target: "noodle::round_trip",
                            error = %e,
                            path = %path.display(),
                            "RoundTripSink: flush failed"
                        );
                    }
                    batched = 0;
                }
            },
            _ = interval.tick() => {
                if batched > 0 {
                    use tokio::io::AsyncWriteExt;
                    let _ = writer.flush().await;
                    batched = 0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use noodle_core::layered::{AuditEvent, AuditKind, Hint, Layer};
    use noodle_core::{Resolved, SessionId, SessionKey};

    fn sid() -> SessionId {
        SessionKey {
            auth_header: b"a",
            session_header: b"b",
        }
        .id()
    }

    fn hint(value: &str) -> SideEffect {
        SideEffect::Hint(Hint {
            category: "tool".into(),
            value: value.into(),
            confidence: 0.9,
            source: "test".into(),
            correlation: None,
        })
    }

    fn audit_err() -> SideEffect {
        SideEffect::Audit(AuditEvent {
            kind: AuditKind::Errored,
            layer: Layer::VendorSemantics,
            transform: "test_transform".into(),
            flow_id: 1,
            at_unix_ms: 0,
            detail: serde_json::json!({"reason": "test"}),
            correlation: None,
        })
    }

    fn resolved_for(flow_id: u64) -> SideEffect {
        SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id,
            at_unix_ms: 0,
            resolved: Resolved::default(),
            correlation: None,
        })
    }

    // ─── InMemorySink ──────────────────────────────────────────

    #[test]
    fn in_memory_sink_records_in_order() {
        let sink = InMemorySink::new();
        sink.record(hint("a"));
        sink.record(audit_err());
        sink.record(hint("b"));
        sink.record(resolved_for(1));

        let snap = sink.snapshot();
        assert_eq!(snap.len(), 4);
        assert!(matches!(snap[0], SideEffect::Hint(ref h) if h.value == "a"));
        assert!(matches!(snap[1], SideEffect::Audit(ref a) if a.kind == AuditKind::Errored));
        assert!(matches!(snap[2], SideEffect::Hint(ref h) if h.value == "b"));
        assert!(matches!(snap[3], SideEffect::Resolved(ref r) if r.flow_id == 1));
    }

    #[test]
    fn in_memory_sink_resolved_records_filter() {
        let sink = InMemorySink::new();
        sink.record(hint("ignored"));
        sink.record(resolved_for(7));
        sink.record(audit_err());
        sink.record(resolved_for(8));

        let resolved = sink.resolved_records();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].flow_id, 7);
        assert_eq!(resolved[1].flow_id, 8);
    }

    #[test]
    fn in_memory_sink_starts_empty() {
        let sink = InMemorySink::new();
        assert!(sink.is_empty());
        assert_eq!(sink.len(), 0);
        assert!(sink.snapshot().is_empty());
    }

    // ─── MultiSideEffectSink ──────────────────────────────────────────

    #[test]
    fn multi_sink_fans_out_to_every_child() {
        let a = Arc::new(InMemorySink::new());
        let b = Arc::new(InMemorySink::new());
        let multi = MultiSideEffectSink::new(vec![
            Arc::clone(&a) as Arc<dyn SideEffectSink>,
            Arc::clone(&b) as Arc<dyn SideEffectSink>,
        ]);

        multi.record(hint("x"));
        multi.record(hint("y"));

        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn multi_sink_continues_when_child_panics() {
        struct PanickingSink;
        impl SideEffectSink for PanickingSink {
            fn record(&self, _effect: SideEffect) {
                panic!("intentional test panic in PanickingSink");
            }
        }

        let panicking = Arc::new(PanickingSink) as Arc<dyn SideEffectSink>;
        let healthy = Arc::new(InMemorySink::new());
        let multi = MultiSideEffectSink::new(vec![
            panicking,
            Arc::clone(&healthy) as Arc<dyn SideEffectSink>,
        ]);

        multi.record(hint("survives"));
        multi.record(hint("also-survives"));

        // The healthy sink received both effects despite the
        // panicking sibling. This is the load-bearing failure-
        // isolation contract from ADR 020 §2.1.
        assert_eq!(healthy.len(), 2);
    }

    #[test]
    fn multi_sink_empty_child_list_is_noop() {
        let multi = MultiSideEffectSink::new(vec![]);
        // No children → no panics, no work.
        multi.record(hint("nothing-to-fan-out-to"));
    }

    // ─── TracingSink ──────────────────────────────────────────────

    #[test]
    fn tracing_sink_records_without_panic_for_every_variant() {
        // We don't assert on tracing output here (would require a
        // test subscriber); we just verify TracingSink accepts
        // every SideEffect variant without panicking.
        let sink = TracingSink;
        sink.record(hint("ok"));
        sink.record(SideEffect::Artifact(noodle_core::layered::Artifact {
            name: "marker".into(),
            value: "v".into(),
            source_layer: Layer::VendorSemantics,
            source_transform: "test".into(),
            flow_id: 1,
            captured_at_unix_ms: 0,
            correlation: None,
        }));
        sink.record(audit_err());
        sink.record(SideEffect::Audit(AuditEvent {
            kind: AuditKind::Enhanced,
            layer: Layer::VendorSemantics,
            transform: "test".into(),
            flow_id: 1,
            at_unix_ms: 0,
            detail: serde_json::json!({}),
            correlation: None,
        }));
        sink.record(resolved_for(99));
    }

    // ─── SideEffectsJsonlSink ──────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jsonl_sink_writes_one_line_per_emission() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("side_effects.jsonl");
        let sink = SideEffectsJsonlSink::spawn(&path).await.expect("spawn");

        sink.record(hint("x"));
        sink.record(audit_err());
        sink.record(resolved_for(7));
        sink.shutdown().await; // drains the writer task

        let contents = std::fs::read_to_string(&path).expect("read jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line).expect("valid JSON");
        }
        assert!(lines[0].contains("\"kind\":\"hint\""));
        assert!(lines[1].contains("\"kind\":\"audit\""));
        assert!(lines[2].contains("\"kind\":\"resolved\""));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jsonl_sink_resolved_carries_session_prefix_and_categories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("se.jsonl");
        let sink = SideEffectsJsonlSink::spawn(&path).await.expect("spawn");

        let mut resolved_map = Resolved::default();
        resolved_map.0.insert("tool".into(), "Claude Code".into());
        sink.record(SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 11,
            at_unix_ms: 1_700_000_000_000,
            resolved: resolved_map,
            correlation: None,
        }));
        sink.shutdown().await;

        let contents = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(contents.trim()).expect("one line of JSON");
        assert_eq!(v["kind"], "resolved");
        assert_eq!(v["flow_id"], 11);
        assert_eq!(v["resolved"]["tool"], "Claude Code");
        let prefix = v["session_prefix"].as_str().expect("session_prefix string");
        assert_eq!(prefix.len(), 8);
        assert!(prefix.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ─── 040.a correlation block ───────────────────────────────────

    /// Build a fully-populated `Correlation` for stamping tests.
    fn corr(event_id: &str) -> noodle_core::layered::Correlation {
        noodle_core::layered::Correlation {
            event_id: event_id.into(),
            turn_id: Some("turn-abc".into()),
            session_id: Some("session-uuid-1234".into()),
            agent_run_id: None,
            at_unix_ms: 1_700_000_000_001,
        }
    }

    #[test]
    fn stamp_correlation_populates_every_variant() {
        // 040.a §3 — the engine drain's stamping seam fills the
        // correlation block on every variant. Bypass-resistant by
        // construction: no other path stamps.
        let mut h = SideEffect::Hint(Hint {
            category: "tool".into(),
            value: "Claude Code".into(),
            confidence: 0.9,
            source: "user_agent".into(),
            correlation: None,
        });
        h.stamp_correlation(corr("nl-1"));
        assert_eq!(h.correlation().expect("stamped").event_id.as_str(), "nl-1");

        let mut a = SideEffect::Artifact(noodle_core::layered::Artifact {
            name: "work_type".into(),
            value: "refactor".into(),
            source_layer: Layer::VendorSemantics,
            source_transform: "marker_strip".into(),
            flow_id: 7,
            captured_at_unix_ms: 0,
            correlation: None,
        });
        a.stamp_correlation(corr("nl-2"));
        assert_eq!(
            a.correlation().expect("stamped").session_id.as_deref(),
            Some("session-uuid-1234")
        );

        let mut au = SideEffect::Audit(AuditEvent {
            kind: AuditKind::Redacted,
            layer: Layer::VendorSemantics,
            transform: "marker_strip".into(),
            flow_id: 7,
            at_unix_ms: 0,
            detail: serde_json::json!({}),
            correlation: None,
        });
        au.stamp_correlation(corr("nl-3"));
        assert_eq!(
            au.correlation().expect("stamped").turn_id.as_deref(),
            Some("turn-abc")
        );

        let mut r = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 7,
            at_unix_ms: 0,
            resolved: Resolved::default(),
            correlation: None,
        });
        r.stamp_correlation(corr("nl-4"));
        assert!(r.correlation().expect("stamped").agent_run_id.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jsonl_sink_writes_correlation_block_on_every_variant() {
        // 040.a AC #1 + #4: every emitted variant carries event_id /
        // turn_id / session_id / agent_run_id / at_unix_ms on disk.
        // session_id is the full MarkingSessionId, not the 8-char
        // hash prefix (AC #2). at_unix_ms is never zero (AC #3).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("se.jsonl");
        let sink = SideEffectsJsonlSink::spawn(&path).await.expect("spawn");

        let stamp = corr("nl-42");

        let mut hint_eff = SideEffect::Hint(Hint {
            category: "tool".into(),
            value: "Claude Code".into(),
            confidence: 1.0,
            source: "user_agent".into(),
            correlation: None,
        });
        hint_eff.stamp_correlation(stamp.clone());
        sink.record(hint_eff);

        let mut artifact_eff = SideEffect::Artifact(noodle_core::layered::Artifact {
            name: "work_type".into(),
            value: "refactor".into(),
            source_layer: Layer::VendorSemantics,
            source_transform: "marker_strip".into(),
            flow_id: 7,
            captured_at_unix_ms: 0,
            correlation: None,
        });
        artifact_eff.stamp_correlation(stamp.clone());
        sink.record(artifact_eff);

        let mut audit_eff = SideEffect::Audit(AuditEvent {
            kind: AuditKind::Redacted,
            layer: Layer::VendorSemantics,
            transform: "marker_strip".into(),
            flow_id: 7,
            at_unix_ms: 0,
            detail: serde_json::json!({}),
            correlation: None,
        });
        audit_eff.stamp_correlation(stamp.clone());
        sink.record(audit_eff);

        let mut resolved_eff = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 7,
            at_unix_ms: stamp.at_unix_ms,
            resolved: Resolved::default(),
            correlation: None,
        });
        resolved_eff.stamp_correlation(stamp.clone());
        sink.record(resolved_eff);

        sink.shutdown().await;

        let contents = std::fs::read_to_string(&path).expect("read jsonl");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 4);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(v["event_id"], "nl-42", "event_id on {line}");
            assert_eq!(v["turn_id"], "turn-abc", "turn_id on {line}");
            assert_eq!(v["session_id"], "session-uuid-1234", "session_id on {line}");
            // The drain-stamped timestamp lives in the variant's
            // native slot: Audit + Resolved use `at_unix_ms`;
            // Artifact uses `captured_at_unix_ms`; Hint gets a
            // dedicated `at_unix_ms` because it has no native
            // slot. Same value across variants — sourced from
            // `correlation.at_unix_ms`.
            let kind = v["kind"].as_str().expect("kind str");
            let ts_field = if kind == "artifact" {
                "captured_at_unix_ms"
            } else {
                "at_unix_ms"
            };
            assert_eq!(
                v[ts_field], 1_700_000_000_001u64,
                "{ts_field} on {kind} record {line}"
            );
            // ADR 052 §5: the correlation carries `frame_id` (renamed
            // from `agent_run_id`). None in this fixture, so
            // skip_serializing_if omits the key.
            assert!(
                v.get("frame_id").is_none(),
                "frame_id absent (None) on {line}"
            );
            assert!(
                v.get("agent_run_id").is_none(),
                "retired agent_run_id key never emitted on {line}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jsonl_sink_omits_correlation_when_unstamped() {
        // Cert-mint AuditEvents bypass the engine drain seam (no
        // inspection flow) — they reach the sink with
        // correlation: None. The sink must accept them and emit a
        // record without the four optional keys.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("se.jsonl");
        let sink = SideEffectsJsonlSink::spawn(&path).await.expect("spawn");

        sink.record(SideEffect::Audit(AuditEvent {
            kind: AuditKind::LeafMinted,
            layer: Layer::Tls,
            transform: "test-mint".into(),
            flow_id: 0,
            at_unix_ms: 1_700_000_000_002,
            detail: serde_json::json!({"host": "example.com"}),
            correlation: None,
        }));
        sink.shutdown().await;

        let contents = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(contents.trim()).expect("one line");
        assert_eq!(v["kind"], "audit");
        assert!(v.get("event_id").is_none(), "no correlation block");
        assert!(v.get("session_id").is_none());
        assert_eq!(v["at_unix_ms"], 1_700_000_000_002u64);
    }

    // ─── 040.b RoundTripSink ─────────────────────────────────────

    /// Fake clock used by `RoundTripSink` unit tests so emitted
    /// `completed_at_unix_ms` is deterministic.
    struct FakeClock {
        millis: u64,
    }
    impl Clock for FakeClock {
        fn now_unix_ms(&self) -> u64 {
            self.millis
        }
    }

    fn make_correlation(event_id: &str) -> noodle_core::layered::Correlation {
        noodle_core::layered::Correlation {
            event_id: event_id.into(),
            turn_id: Some("turn-xyz".into()),
            session_id: Some("session-uuid-aaa".into()),
            agent_run_id: None,
            at_unix_ms: 1_700_000_000_100,
        }
    }

    fn make_request_wire(request_id: &str, ts: u64) -> noodle_core::WireEvent {
        noodle_core::WireEvent {
            direction: noodle_core::WireDirection::Request,
            request_id: request_id.into(),
            ts_unix_ms: ts,
            method: Some("POST".into()),
            url: Some("https://api.anthropic.com/v1/messages".to_string()),
            status: None,
            headers: vec![
                noodle_core::HeaderPair {
                    name: "user-agent".into(),
                    value: "Claude-Code/2.1.0".into(),
                },
                noodle_core::HeaderPair {
                    name: "host".into(),
                    value: "api.anthropic.com".into(),
                },
            ],
            body_in: bytes::Bytes::from(r#"{"model":"claude-3-5-sonnet-20241022","messages":[]}"#),
            body_out: bytes::Bytes::from(
                r#"{"model":"claude-3-5-sonnet-20241022","messages":[],"system":"enhanced"}"#,
            ),
            marks: None,
            provider: Some("anthropic".into()),
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

    fn make_response_wire(request_id: &str, ts: u64) -> noodle_core::WireEvent {
        noodle_core::WireEvent {
            direction: noodle_core::WireDirection::Response,
            request_id: request_id.into(),
            ts_unix_ms: ts,
            method: None,
            url: None,
            status: Some(200),
            headers: vec![noodle_core::HeaderPair {
                name: "content-type".into(),
                value: "text/event-stream".into(),
            }],
            body_in: bytes::Bytes::from(
                r#"event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

"#,
            ),
            body_out: bytes::Bytes::new(),
            marks: None,
            provider: Some("anthropic".into()),
            agent_app: None,
            machine: None,
            collector_app: None,
            subscription: None,
            usage: Some(noodle_core::WireUsage {
                tokens: Some(noodle_core::WireTokenUsage {
                    input: 100,
                    output: 200,
                    cached_read: Some(50),
                    cached_creation: Some(10),
                    reasoning: None,
                    cache_creation: Some(noodle_core::CacheCreationTtl {
                        ephemeral_5m_input_tokens: Some(7),
                        ephemeral_1h_input_tokens: Some(3),
                    }),
                    vendor_extras: std::collections::BTreeMap::new(),
                }),
                latency: Some(noodle_core::WireLatency {
                    time_to_first_byte_ms: Some(42),
                    total_ms: Some(987),
                }),
                service_tier: Some("standard".into()),
                inference_geo: Some("us-east-1".into()),
            }),
            content_blocks: None,
            events: None,
            pairing: None,
            attribution: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_sink_assembles_one_record_per_flow() {
        // AC #1 + #2: one JSONL line per round trip with the
        // ADR 023 §4 schema shape. AC #3: four correlation IDs
        // present (turn_id + session_id from the marking detector;
        // frame_id per ADR 052 §5, None here; event_id always).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roundtrips.jsonl");
        let clock: Arc<dyn Clock> = Arc::new(FakeClock {
            millis: 1_700_000_000_500,
        });
        let sink = RoundTripSink::spawn(&path, clock).await.expect("spawn");

        noodle_core::WireSink::record(&sink, make_request_wire("nl-7", 1_700_000_000_000));
        // 1st Resolved — the request-side drain
        // (`request_outbound`'s engine flow), carrying the UA Hint.
        let mut hint = SideEffect::Hint(Hint {
            category: "tool".into(),
            value: "Claude Code".into(),
            confidence: 1.0,
            source: "user_agent".into(),
            correlation: None,
        });
        hint.stamp_correlation(make_correlation("nl-7"));
        sink.record(hint);
        let mut request_resolved = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 0,
            at_unix_ms: 1_700_000_000_050,
            resolved: Resolved::default(),
            correlation: None,
        });
        request_resolved.stamp_correlation(make_correlation("nl-7"));
        sink.record(request_resolved);

        // 2nd Resolved — the response-side drain
        // (`EngineState::finish`), carrying the marker Artifact
        // + attribution. This is the second Resolved that gates
        // emission per the LLM-round-trip filter.
        let mut artifact = SideEffect::Artifact(noodle_core::layered::Artifact {
            name: "work_type".into(),
            value: "refactor".into(),
            source_layer: Layer::VendorSemantics,
            source_transform: "marker_strip".into(),
            flow_id: 7,
            captured_at_unix_ms: 0,
            correlation: None,
        });
        artifact.stamp_correlation(make_correlation("nl-7"));
        sink.record(artifact);
        let mut resolved = {
            let mut r = Resolved::default();
            r.0.insert("tool".into(), "Claude Code".into());
            SideEffect::Resolved(ResolvedRecord {
                session: sid(),
                flow_id: 7,
                at_unix_ms: 1_700_000_000_100,
                resolved: r,
                correlation: None,
            })
        };
        resolved.stamp_correlation(make_correlation("nl-7"));
        sink.record(resolved);
        noodle_core::WireSink::record(&sink, make_response_wire("nl-7", 1_700_000_000_400));

        sink.shutdown().await;

        let contents = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1, "expected exactly one round-trip record");
        let v: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON");

        assert_eq!(v["kind"], "round_trip");
        assert_eq!(v["event_id"], "nl-7");
        assert_eq!(v["turn_id"], "turn-xyz");
        assert_eq!(v["session_id"], "session-uuid-aaa");
        // ADR 052 §5: frame_id replaces agent_run_id on the
        // correlation. None in this fixture → key omitted.
        assert!(v.get("frame_id").is_none(), "frame_id None in fixture");
        assert!(
            v.get("agent_run_id").is_none(),
            "retired agent_run_id key never emitted"
        );
        assert_eq!(v["flow_id"], 7);
        assert_eq!(v["started_at_unix_ms"], 1_700_000_000_000u64);
        assert_eq!(v["completed_at_unix_ms"], 1_700_000_000_400u64);
        assert_eq!(v["duration_ms"], 400);

        assert_eq!(v["request"]["host"], "api.anthropic.com");
        assert_eq!(v["request"]["endpoint"], "/v1/messages");
        assert_eq!(v["request"]["method"], "POST");
        assert_eq!(v["request"]["user_agent"], "Claude-Code/2.1.0");
        assert_eq!(v["request"]["model"], "claude-3-5-sonnet-20241022");
        assert_eq!(v["request"]["directive_enhanced"], true);

        assert_eq!(v["response"]["status"], 200);
        assert_eq!(v["response"]["kind"], "sse");
        assert_eq!(v["response"]["complete"], true);
        assert_eq!(v["response"]["stop_reason"], "end_turn");

        assert_eq!(v["attributions"]["tool"], "Claude Code");

        // AC #8: service_tier + inference_geo as siblings of
        // `tokens`; nested cache_creation TTL breakdown.
        assert_eq!(v["usage"]["service_tier"], "standard");
        assert_eq!(v["usage"]["inference_geo"], "us-east-1");
        assert_eq!(v["usage"]["tokens"]["input_tokens"], 100);
        assert_eq!(
            v["usage"]["tokens"]["cache_creation"]["ephemeral_5m_input_tokens"],
            7
        );
        assert_eq!(
            v["usage"]["tokens"]["cache_creation"]["ephemeral_1h_input_tokens"],
            3
        );

        // Evidence: the contributing Hint + Artifact.
        let hints = v["evidence"]["hints"].as_array().expect("hints array");
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0]["category"], "tool");
        let artifacts = v["evidence"]["artifacts"].as_array().expect("artifacts");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0]["name"], "work_type");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_sink_handles_resolved_arriving_before_response() {
        // Drain fires (Resolved on side-effect bus) BEFORE the
        // response WireEvent in TeeBody::poll_frame — same order
        // production hits. Verify the sink doesn't emit on
        // Resolved alone; emission is gated on the response wire
        // event arriving after.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("roundtrips.jsonl");
        let clock: Arc<dyn Clock> = Arc::new(FakeClock {
            millis: 1_700_000_000_500,
        });
        let sink = RoundTripSink::spawn(&path, clock).await.expect("spawn");

        noodle_core::WireSink::record(&sink, make_request_wire("nl-9", 1_700_000_000_000));
        // Resolved arrives before response wire event.
        let mut resolved = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 9,
            at_unix_ms: 1_700_000_000_100,
            resolved: Resolved::default(),
            correlation: None,
        });
        resolved.stamp_correlation(make_correlation("nl-9"));
        sink.record(resolved);

        // Mid-state: file should be empty so far.
        sink.shutdown().await;
        let pre = std::fs::read_to_string(&path).expect("read");
        let pre_lines: Vec<&str> = pre.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            pre_lines.len(),
            0,
            "no record should be emitted before the response WireEvent"
        );

        // Restart the sink for the response-arrives leg. (The
        // sink's contract is one shutdown per instance; the
        // emission-gate behaviour itself is what we want to
        // assert. Mid-flow checking is the assertion above.)
        let clock2: Arc<dyn Clock> = Arc::new(FakeClock {
            millis: 1_700_000_000_500,
        });
        let sink2 = RoundTripSink::spawn(&path, clock2).await.expect("spawn 2");
        noodle_core::WireSink::record(&sink2, make_request_wire("nl-9b", 1_700_000_000_000));
        // Request-side drain.
        let mut req_resolved = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 0,
            at_unix_ms: 1_700_000_000_050,
            resolved: Resolved::default(),
            correlation: None,
        });
        req_resolved.stamp_correlation(make_correlation("nl-9b"));
        sink2.record(req_resolved);
        // Response-side drain.
        let mut resolved2 = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 99,
            at_unix_ms: 1_700_000_000_100,
            resolved: Resolved::default(),
            correlation: None,
        });
        resolved2.stamp_correlation(make_correlation("nl-9b"));
        sink2.record(resolved2);
        noodle_core::WireSink::record(&sink2, make_response_wire("nl-9b", 1_700_000_000_300));
        sink2.shutdown().await;

        let post = std::fs::read_to_string(&path).expect("read 2");
        let post_lines: Vec<&str> = post.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            post_lines.len(),
            1,
            "exactly one record after response wire event"
        );
        let v: serde_json::Value = serde_json::from_str(post_lines[0]).expect("valid JSON");
        assert_eq!(v["event_id"], "nl-9b");
        assert_eq!(v["flow_id"], 99);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_sink_evidence_cap_truncates_with_audit_marker() {
        // AC #6 / AC #7 — bounded per-flow buffer. Flood the sink
        // with more than the per-flow cap; assert truncation
        // emits a synthetic Filtered audit and that the resulting
        // record memory is bounded.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rt.jsonl");
        let clock: Arc<dyn Clock> = Arc::new(FakeClock {
            millis: 1_700_000_000_500,
        });
        let sink = RoundTripSink::spawn(&path, clock).await.expect("spawn");

        noodle_core::WireSink::record(&sink, make_request_wire("nl-flood", 1_700_000_000_000));
        // Request-side drain.
        let mut req = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 0,
            at_unix_ms: 1_700_000_000_050,
            resolved: Resolved::default(),
            correlation: None,
        });
        req.stamp_correlation(make_correlation("nl-flood"));
        sink.record(req);
        for i in 0..(ROUND_TRIP_MAX_EVIDENCE_PER_FLOW + 50) {
            let mut h = SideEffect::Hint(Hint {
                category: "tool".into(),
                value: format!("v{i}").into(),
                confidence: 0.5,
                source: "test".into(),
                correlation: None,
            });
            h.stamp_correlation(make_correlation("nl-flood"));
            sink.record(h);
        }
        // Response-side drain.
        let mut resolved = SideEffect::Resolved(ResolvedRecord {
            session: sid(),
            flow_id: 5,
            at_unix_ms: 1_700_000_000_100,
            resolved: Resolved::default(),
            correlation: None,
        });
        resolved.stamp_correlation(make_correlation("nl-flood"));
        sink.record(resolved);
        noodle_core::WireSink::record(&sink, make_response_wire("nl-flood", 1_700_000_000_400));
        sink.shutdown().await;

        let contents = std::fs::read_to_string(&path).expect("read");
        let v: serde_json::Value = serde_json::from_str(contents.trim()).expect("valid JSON");
        let hints = v["evidence"]["hints"].as_array().expect("hints");
        assert!(
            hints.len() <= ROUND_TRIP_MAX_EVIDENCE_PER_FLOW,
            "per-flow cap honoured"
        );
        // The synthetic Filtered audit must be present so
        // downstream consumers detect truncation.
        let audits = v["evidence"]["audits"].as_array().expect("audits");
        let has_cap_audit = audits.iter().any(|a| {
            a["kind_inner"] == "filtered"
                && a["transform"] == "round_trip_sink"
                && a["detail"]["reason"] == "per_flow_evidence_cap_reached"
        });
        assert!(
            has_cap_audit,
            "truncation must emit a Filtered audit, got: {audits:#?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn round_trip_sink_ignores_effects_without_correlation() {
        // Cert-mint audits (no inspection flow) carry no
        // correlation block. They should NOT land in
        // roundtrips.jsonl — that file is per-round-trip only.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rt.jsonl");
        let clock: Arc<dyn Clock> = Arc::new(FakeClock {
            millis: 1_700_000_000_500,
        });
        let sink = RoundTripSink::spawn(&path, clock).await.expect("spawn");

        sink.record(SideEffect::Audit(AuditEvent {
            kind: AuditKind::LeafMinted,
            layer: Layer::Tls,
            transform: "test-mint".into(),
            flow_id: 0,
            at_unix_ms: 1_700_000_000_002,
            detail: serde_json::json!({"host": "example.com"}),
            correlation: None,
        }));
        sink.shutdown().await;

        let contents = std::fs::read_to_string(&path).expect("read");
        assert!(
            contents.is_empty(),
            "non-flow audit must not land in roundtrips.jsonl: got {contents}"
        );
    }
}
