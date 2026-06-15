//! Tail `~/.noodle/tap.jsonl` (or any JSONL file) and emit one parsed
//! [`Exchange`] per complete line.
//!
//! Implementation note (S15 of the 027–031 refactor; refactor-overview
//! §2): the underlying reader is
//! [`noodle_tap::source::FileTail`](`noodle_tap::source::FileTail`), the
//! `WireSource` impl introduced in S12. This crate no longer maintains
//! its own fsnotify+poll tail loop — every consumer of `tap.jsonl` now
//! goes through the same boundary the proxy writes to.
//!
//! The viewer-side adapter is a thin bridge that:
//!
//! 1. Spawns the sync `FileTail` on a blocking worker (its
//!    `next_record` blocks waiting for the writer to flush a line).
//! 2. Deserialises each `serde_json::Value` into the viewer's
//!    [`Exchange`] (the wire shape the frontend already understands).
//! 3. Forwards through a bounded `mpsc::Sender` so back-pressure
//!    propagates back through the tail when consumers fall behind.
//!
//! ## Behaviour preserved from the prior implementation
//!
//! - Initial replay of the existing file (live-tail default per
//!   `FileTail::open` — seeks to start-of-file).
//! - Unparseable lines logged and skipped, never fatal.
//! - File-not-yet-exist handled by polling for the file to appear
//!   before opening the tail (writer task may not have created the
//!   file by the time the viewer starts on cold launch).
//!
//! ## Differences from the prior implementation
//!
//! - **No fsnotify.** `FileTail` polls at a fixed interval (50ms by
//!   default; same order of magnitude as the prior 500ms poll-safety
//!   net). Sub-100ms latency in steady state.
//! - **Truncation handling.** `FileTail` does not currently rewind on
//!   truncation (proxy restarts during a live viewer session are
//!   rare; an explicit reconnect-on-restart is future work for the
//!   tail itself rather than the adapter).
//! - **`Record = serde_json::Value`.** The tail boundary stays
//!   loosely typed; this adapter is the strongly-typed deserialise
//!   point.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use noodle_core::WireSource;
use noodle_tap::source::{CloseHandle, FileTail, FileTailError};
use tokio::sync::mpsc;

use crate::decoders::ProviderDecoderRegistry;
use crate::model::{DecodedExchange, Exchange};
use crate::ports::{DecodedExchangeSource, EventSource};

/// How long to wait for the tap file to appear before giving up.
/// On cold start the writer task creates the file inside
/// `TapJsonlLog::spawn`, which is normally before the viewer
/// starts. This is the belt-and-suspenders bound when the order is
/// reversed (viewer first, proxy second).
const FILE_APPEAR_TIMEOUT: Duration = Duration::from_secs(5);

/// Probe interval while waiting for the file to exist.
const FILE_APPEAR_POLL: Duration = Duration::from_millis(50);

/// `WireSource::FileTail`-backed tap.jsonl source.
///
/// Construct via [`Self::spawn`]. The tail runs on a dedicated OS
/// thread (`std::thread::spawn`) — `FileTail::next_record` is sync
/// and blocks waiting for the writer to flush a line, so a plain
/// thread is the right home for it. Events are pushed on the
/// receiver returned by [`EventSource::subscribe`].
///
/// The tail worker keeps running for the lifetime of the receiver —
/// dropping `TapJsonlSource` itself does NOT stop the tail (matching
/// the prior implementation's "watcher lives in the spawned task"
/// shape). The worker exits when:
///
/// - The receiver returned by [`EventSource::subscribe`] is dropped
///   (the worker notices on its next iteration via `tx.is_closed()`
///   or on the next `blocking_send`).
/// - [`Self::close`] is called — the worker checks the close flag at
///   the top of every poll iteration.
///
/// Tests that don't drop the receiver before the runtime shuts down
/// should call [`Self::close`] to release the worker thread.
pub struct TapJsonlSource {
    rx: std::sync::Mutex<Option<mpsc::Receiver<Exchange>>>,
    close_handle: CloseHandle,
    /// Background OS thread running the `FileTail` polling loop.
    /// Held in an Option so [`Self::close`] can take and join it
    /// without consuming `self`.
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TapJsonlSource {
    /// Begin tailing `path`. Returns once the underlying file has been
    /// opened and the tail worker has been spawned. Initial replay
    /// (any lines already in the file) happens asynchronously on the
    /// worker after this returns.
    ///
    /// `capacity` bounds the mpsc channel the tail writes into. Slow
    /// consumers exert back-pressure on the tail; on full channel,
    /// the worker blocks rather than dropping records.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`std::io::Error`] when the file cannot
    /// be opened within [`FILE_APPEAR_TIMEOUT`] (does not exist,
    /// permission denied, etc.).
    pub async fn spawn(path: PathBuf, capacity: usize) -> std::io::Result<Self> {
        // Wait briefly for the file to appear. The writer task creates
        // it synchronously inside its spawn, but on cold startup the
        // viewer may be racing the proxy.
        wait_for_file(&path).await?;

        // Open the tail synchronously here so any I/O error surfaces on
        // the caller's await, not in the background.
        let tail = FileTail::open(&path)?;
        let close_handle = tail.close_handle();

        let (tx, rx) = mpsc::channel::<Exchange>(capacity);
        let worker = spawn_tail_worker(tail, tx);

        Ok(Self {
            rx: std::sync::Mutex::new(Some(rx)),
            close_handle,
            worker: std::sync::Mutex::new(Some(worker)),
        })
    }

    /// Stop the tail worker and wait for the background thread to
    /// exit. After this returns, no more records will be sent on the
    /// receiver — calling it twice is a no-op.
    ///
    /// Idiomatic at shutdown: ensures the tokio runtime can finish
    /// dropping (the worker held an open `FileTail` file handle and
    /// was sleeping inside `thread::sleep`; without this, runtime
    /// shutdown can hang waiting for the worker to wake).
    pub fn close(&self) {
        // Signal the FileTail to stop polling on its next iteration.
        self.close_handle.close();
        // Join the worker thread so the caller has a hard ordering
        // guarantee that no more sends occur after `close` returns.
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

impl EventSource for TapJsonlSource {
    fn subscribe(&self) -> mpsc::Receiver<Exchange> {
        self.rx
            .lock()
            .expect("poisoned")
            .take()
            .expect("TapJsonlSource::subscribe called more than once")
    }
}

/// Block (up to [`FILE_APPEAR_TIMEOUT`]) waiting for `path` to exist.
async fn wait_for_file(path: &PathBuf) -> std::io::Result<()> {
    let deadline = tokio::time::Instant::now() + FILE_APPEAR_TIMEOUT;
    loop {
        if tokio::fs::metadata(path).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("tap.jsonl did not appear at {}", path.display()),
            ));
        }
        tokio::time::sleep(FILE_APPEAR_POLL).await;
    }
}

/// Spawn the blocking worker that pumps records from `FileTail` to the
/// mpsc channel.
///
/// Returns a [`std::thread::JoinHandle`] so callers can block-wait on
/// the worker exit if desired. The worker is run on a dedicated OS
/// thread (via `std::thread::spawn`) rather than on tokio's blocking
/// pool — the prior shape pinned a tokio blocking thread for the
/// lifetime of the source, but it could not be joined synchronously
/// inside [`TapJsonlSource`]'s `close` path. A plain `std::thread`
/// gives us a `JoinHandle` to wait on at shutdown so tests don't hang.
///
/// Each record is deserialised into [`Exchange`]; on deserialise
/// failure the line is logged and skipped.
fn spawn_tail_worker(
    mut tail: FileTail,
    tx: mpsc::Sender<Exchange>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            // Fast path: if the receiver has dropped, exit before
            // doing more I/O. Saves one poll-interval of latency at
            // shutdown.
            if tx.is_closed() {
                return;
            }
            match tail.next_record() {
                Ok(Some(value)) => {
                    // LEGACY — read marks.turn_id instead, retire
                    // when S22 lands. This slim-Exchange path
                    // feeds the React frontend's `ooda.ts`
                    // heuristic; the S21 typed path through
                    // [`DecodedTapJsonlSource`] +
                    // [`ProviderDecoderRegistry`] is the
                    // forward-looking surface (refactor-overview
                    // §10). Serde drops unknown fields here so
                    // envelope.* / content.blocks[] / events[] /
                    // pairing keep flowing through; the React
                    // client still reads them off the raw `body`
                    // until S22 swaps it to consume
                    // `DecodedExchange`.
                    let ex: Exchange = match serde_json::from_value(value) {
                        Ok(ex) => ex,
                        Err(e) => {
                            tracing::warn!(
                                ?e,
                                "tap tail: skipping line that failed to deserialise into Exchange"
                            );
                            continue;
                        }
                    };
                    if tx.blocking_send(ex).is_err() {
                        // Receiver dropped — orderly shutdown.
                        return;
                    }
                }
                Ok(None) => {
                    // Tail mode never returns Ok(None), but be
                    // tolerant. If a future FileTail revision adds
                    // EOF semantics we exit cleanly.
                    return;
                }
                Err(FileTailError::Closed) => return,
                Err(e) => {
                    // Parse failures and transient I/O errors. Log
                    // and exit: the underlying source has signalled a
                    // fault that we can't recover from without
                    // re-opening the file (proxy restart, truncation,
                    // etc.). The viewer can be relaunched.
                    tracing::warn!(?e, "tap tail: error from FileTail; worker exiting");
                    return;
                }
            }
        }
    })
}

// ────────────────────────────────────────────────────────────────
// S21: DecodedTapJsonlSource — same FileTail, typed output
// ────────────────────────────────────────────────────────────────

/// Typed sibling to [`TapJsonlSource`] (S21 of the 027–031
/// refactor; refactor-overview.md §10). Tails the same
/// `tap.jsonl` file but dispatches every record through a
/// [`ProviderDecoderRegistry`] before forwarding, so consumers
/// receive a typed [`DecodedExchange`] carrying marks / envelope /
/// usage / `content_blocks` / events / pairing — not just the slim
/// [`Exchange`].
///
/// Construct via [`Self::spawn`]. Lifecycle and shutdown semantics
/// mirror [`TapJsonlSource`] exactly — including the `close()`
/// call required at end of test to join the blocking worker
/// thread.
pub struct DecodedTapJsonlSource {
    rx: std::sync::Mutex<Option<mpsc::Receiver<DecodedExchange>>>,
    close_handle: CloseHandle,
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl DecodedTapJsonlSource {
    /// Begin tailing `path`. Returns once the file has been opened
    /// and the tail worker has been spawned. Uses the default
    /// [`ProviderDecoderRegistry`] (anthropic-only today).
    ///
    /// # Errors
    ///
    /// See [`TapJsonlSource::spawn`].
    pub async fn spawn(path: PathBuf, capacity: usize) -> std::io::Result<Self> {
        Self::spawn_with_registry(path, capacity, ProviderDecoderRegistry::with_defaults()).await
    }

    /// Begin tailing `path` with a caller-supplied registry. Useful
    /// when tests want to register a custom decoder, or when a
    /// future deployment registers openai / google decoders.
    ///
    /// # Errors
    ///
    /// See [`TapJsonlSource::spawn`].
    pub async fn spawn_with_registry(
        path: PathBuf,
        capacity: usize,
        registry: ProviderDecoderRegistry,
    ) -> std::io::Result<Self> {
        wait_for_file(&path).await?;
        let tail = FileTail::open(&path)?;
        let close_handle = tail.close_handle();
        let (tx, rx) = mpsc::channel::<DecodedExchange>(capacity);
        let worker = spawn_decoded_tail_worker(tail, tx, Arc::new(registry));
        Ok(Self {
            rx: std::sync::Mutex::new(Some(rx)),
            close_handle,
            worker: std::sync::Mutex::new(Some(worker)),
        })
    }

    /// Stop the tail worker and wait for the background thread to
    /// exit. Idempotent.
    pub fn close(&self) {
        self.close_handle.close();
        if let Ok(mut guard) = self.worker.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

impl DecodedExchangeSource for DecodedTapJsonlSource {
    fn subscribe(&self) -> mpsc::Receiver<DecodedExchange> {
        self.rx
            .lock()
            .expect("poisoned")
            .take()
            .expect("DecodedTapJsonlSource::subscribe called more than once")
    }
}

/// Worker for [`DecodedTapJsonlSource`]. Identical lifecycle to
/// [`spawn_tail_worker`] above — drops malformed lines, exits
/// cleanly on close / receiver-drop — but pipes each record
/// through the [`ProviderDecoderRegistry`] before sending.
fn spawn_decoded_tail_worker(
    mut tail: FileTail,
    tx: mpsc::Sender<DecodedExchange>,
    registry: Arc<ProviderDecoderRegistry>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            if tx.is_closed() {
                return;
            }
            match tail.next_record() {
                Ok(Some(value)) => {
                    let Some(decoded) = registry.decode(&value) else {
                        // Malformed line — same drop policy as the
                        // slim worker. The registry has already
                        // logged the parse failure (it goes through
                        // the same `serde_json::from_value` path).
                        continue;
                    };
                    if tx.blocking_send(decoded).is_err() {
                        return;
                    }
                }
                Ok(None) | Err(FileTailError::Closed) => return,
                Err(e) => {
                    tracing::warn!(
                        ?e,
                        "tap tail (decoded): error from FileTail; worker exiting"
                    );
                    return;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;
    use tokio::time::Duration as TDuration;

    /// Lines already on disk before the source spawns are replayed
    /// from offset 0. Matches the prior `replay_initial` behaviour.
    #[tokio::test]
    async fn replays_existing_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let line = r#"{"direction":"request","timestamp":"t0","event_id":"nl-0","provider":"x"}"#;
        tokio::fs::write(&path, format!("{line}\n")).await.unwrap();

        let source = TapJsonlSource::spawn(path.clone(), 16).await.unwrap();
        let mut rx = source.subscribe();

        let ev = tokio::time::timeout(TDuration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("event");
        assert_eq!(ev.event_id, "nl-0");

        // Stop the blocking worker so the tokio runtime can shut
        // down cleanly at the end of the test.
        source.close();
    }

    /// Pre-existing line + an appended line both surface. Tests the
    /// live-tail path through `FileTail` (sleep + retry inside
    /// `next_record`).
    #[tokio::test]
    async fn replays_existing_then_picks_up_new_lines() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let pre = r#"{"direction":"request","timestamp":"t0","event_id":"nl-0","provider":"x"}"#;
        tokio::fs::write(&path, format!("{pre}\n")).await.unwrap();

        let source = TapJsonlSource::spawn(path.clone(), 16).await.unwrap();
        let mut rx = source.subscribe();

        let ev = tokio::time::timeout(TDuration::from_millis(500), rx.recv())
            .await
            .expect("timeout pre")
            .expect("event pre");
        assert_eq!(ev.event_id, "nl-0");

        // Append a new line; tail's poll loop should pick it up
        // within one poll interval (~50ms default).
        let mut f = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        let new = r#"{"direction":"response","timestamp":"t1","event_id":"nl-0","provider":"x"}"#;
        f.write_all(format!("{new}\n").as_bytes()).await.unwrap();
        f.flush().await.unwrap();

        let ev2 = tokio::time::timeout(TDuration::from_secs(2), rx.recv())
            .await
            .expect("timeout post")
            .expect("event post");
        assert_eq!(ev2.event_id, "nl-0");
        assert!(matches!(ev2.direction, crate::model::Direction::Response));
        source.close();
    }

    /// A line that's syntactically JSON but does not deserialise into
    /// `Exchange` (missing required fields) is logged and skipped —
    /// not fatal. Subsequent good lines still arrive.
    #[tokio::test]
    async fn unparseable_into_exchange_skipped_not_fatal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        // First line is valid JSON but lacks `direction` and
        // `event_id` (required for `Exchange`). FileTail accepts it
        // as `Value`; the adapter rejects it on deserialise.
        let pre = format!(
            "{}\n{}\n",
            r#"{"only":"junk"}"#,
            r#"{"direction":"request","timestamp":"t0","event_id":"nl-0","provider":"x"}"#
        );
        tokio::fs::write(&path, pre).await.unwrap();

        let source = TapJsonlSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();

        let ev = tokio::time::timeout(TDuration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("event");
        assert_eq!(ev.event_id, "nl-0");
        source.close();
    }

    /// `tap.jsonl` records carry many fields beyond the viewer's
    /// `Exchange` shape (envelope.*, content.blocks[], events[],
    /// pairing). Serde must ignore unknown fields — the line still
    /// deserialises and surfaces on the receiver.
    ///
    /// This pins the contract that S4–S11 envelope/decoded-layer
    /// fields don't break the viewer when they appear on the wire.
    #[tokio::test]
    async fn ignores_unknown_fields_on_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        // Realistic shape: envelope + content + events + pairing.
        let line = r#"{"direction":"response","timestamp":"t0","event_id":"nl-1","provider":"anthropic","status":200,"envelope":{"agent_app":{"id":"claude-code"}},"content":{"blocks":[{"kind":"text","text":"hi"}]},"events":[{"type":"message_start","ts_offset_ms":0}],"pairing":{"resolves_tool_use_in_request_id":"nl-0"}}"#;
        tokio::fs::write(&path, format!("{line}\n")).await.unwrap();

        let source = TapJsonlSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();

        let ev = tokio::time::timeout(TDuration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("event");
        assert_eq!(ev.event_id, "nl-1");
        assert_eq!(ev.provider, "anthropic");
        assert_eq!(ev.status, Some(200));
        assert!(matches!(ev.direction, crate::model::Direction::Response));
        source.close();
    }

    /// Opening a tail on a path that never appears within the timeout
    /// returns a clean `NotFound` error. Prior behaviour was to spawn
    /// a watcher that would silently swallow the missing file; this
    /// is a deliberate change — the viewer should fail loud rather
    /// than appear to work.
    #[tokio::test]
    async fn missing_path_yields_clean_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("never-exists.jsonl");
        let result = TapJsonlSource::spawn(path, 16).await;
        let Err(err) = result else {
            panic!("expected NotFound, got Ok");
        };
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    // ────────────────────────────────────────────────────────────
    // S21: DecodedTapJsonlSource unit tests
    // ────────────────────────────────────────────────────────────

    /// A pre-existing tap.jsonl line carrying a typed envelope +
    /// marks + decoded content + usage decodes through the
    /// [`DecodedTapJsonlSource`] into a [`DecodedExchange`] with
    /// every typed field populated. Pins the in-process wiring
    /// without spawning a real proxy — the e2e test below does the
    /// end-to-end exercise.
    #[tokio::test]
    async fn decoded_source_emits_typed_decoded_exchange_for_pre_existing_record() {
        use noodle_domain::decoders::DecodedEvent;

        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        // One anthropic response record carrying every decoded-
        // layer field S21 needs to surface: marks, envelope,
        // usage, content.blocks (text + tool_use), events,
        // pairing.
        let line = r#"{
            "direction": "response",
            "timestamp": "2026-05-21T00:00:01Z",
            "event_id": "nl-7",
            "provider": "anthropic",
            "status": 200,
            "marks": {"session_id": "sess_a", "turn_id": "turn_a"},
            "envelope": {
                "agent_app": {"name": "claude_code", "version": "0.2.5",
                              "build_hash": null, "build_date": null,
                              "source": "user_agent_header"},
                "collector_app": {"name": "noodle", "version": "0.0.1",
                                  "build_hash": "abcd", "build_date": "2026-05-21T00:00:00Z",
                                  "features": ["tap"]}
            },
            "content": { "blocks": [
                { "kind": "text", "text": "Hi." },
                { "kind": "tool_use", "tool_use_id": "toolu_a",
                  "tool_name": "Read", "input": {"file_path": "/x"} }
            ]},
            "events": [
                { "ts_offset_ms": 0, "type": "message_start" },
                { "ts_offset_ms": 10, "type": "message_delta",
                  "delta": { "stop_reason": "end_turn" } }
            ],
            "usage": {
                "tokens": { "input_tokens": 12, "output_tokens": 5 },
                "latency": { "time_to_first_byte_ms": 42, "total_ms": 987 }
            }
        }"#;
        // jsonl is one record per line — flatten the pretty JSON.
        let compact: serde_json::Value = serde_json::from_str(line).unwrap();
        let one_line = serde_json::to_string(&compact).unwrap();
        tokio::fs::write(&path, format!("{one_line}\n"))
            .await
            .unwrap();

        let source = DecodedTapJsonlSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();

        let dx = tokio::time::timeout(TDuration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("event");

        // Slim Exchange preserved.
        assert_eq!(dx.exchange.event_id, "nl-7");
        assert_eq!(dx.exchange.provider, "anthropic");

        // Typed marks
        let marks = dx.marks.as_ref().expect("marks populated");
        assert_eq!(marks.session_id.as_str(), "sess_a");
        assert_eq!(marks.turn_id.as_ref().unwrap().as_str(), "turn_a");

        // Typed envelope
        let env = dx.envelope.as_ref().expect("envelope populated");
        let collector = env.collector_app.as_ref().expect("collector_app");
        assert_eq!(collector.name, "noodle");
        let agent = env.agent_app.as_ref().expect("agent_app");
        assert_eq!(
            agent.name,
            noodle_domain::observation_context::AgentAppName::ClaudeCode
        );

        // Typed usage
        let usage = dx.usage.as_ref().expect("usage populated");
        let tokens = usage.tokens.as_ref().expect("tokens");
        assert_eq!(tokens.input, 12);
        assert_eq!(tokens.output, 5);

        // Decoded content blocks: Content (text) + ToolUse + TurnEnd
        assert_eq!(dx.content_blocks.len(), 3);
        assert!(matches!(dx.content_blocks[0], DecodedEvent::Content { .. }));
        assert!(matches!(dx.content_blocks[1], DecodedEvent::ToolUse { .. }));
        assert!(matches!(dx.content_blocks[2], DecodedEvent::TurnEnd { .. }));

        // Raw events preserved verbatim.
        assert_eq!(dx.events.len(), 2);

        source.close();
    }

    /// Records whose `provider` is unknown to the registry still
    /// produce a [`DecodedExchange`] — the typed envelope / marks
    /// fields come through, just `content_blocks` is empty. Pins
    /// the "passthrough" behaviour S21 demands.
    #[tokio::test]
    async fn decoded_source_passes_through_unknown_provider() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("tap.jsonl");
        let line = r#"{"direction":"request","timestamp":"t","event_id":"nl-x","provider":"future_vendor","marks":{"session_id":"s","turn_id":"t"}}"#;
        tokio::fs::write(&path, format!("{line}\n")).await.unwrap();

        let source = DecodedTapJsonlSource::spawn(path, 16).await.unwrap();
        let mut rx = source.subscribe();

        let dx = tokio::time::timeout(TDuration::from_millis(500), rx.recv())
            .await
            .expect("timeout")
            .expect("event");

        assert_eq!(dx.exchange.provider, "future_vendor");
        assert!(
            dx.content_blocks.is_empty(),
            "unknown provider ⇒ no decoded content blocks"
        );
        let marks = dx.marks.expect("marks populated");
        assert_eq!(marks.session_id.as_str(), "s");

        source.close();
    }
}
