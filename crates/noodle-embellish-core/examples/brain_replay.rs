//! Replay a `tap.jsonl` file through `Brain` and print per-thread
//! observations. The sanity check that ADR 047 rung 1 matches what
//! the offline python analyzer found on the E1 captured corpus
//! (see `notes/e1-compaction-evidence.md`).
//!
//! Usage:
//!
//! ```sh
//! cargo run --example brain_replay -p noodle-embellish-core -- /tmp/noodle-tap.jsonl
//! ```

use std::path::PathBuf;

use noodle_embellish_core::{Brain, TapEntryView, read_tap_jsonl};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/noodle-tap.jsonl".to_owned());
    let entries = match read_tap_jsonl(&PathBuf::from(&path)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("read {path}: {e}");
            std::process::exit(2);
        }
    };
    eprintln!("# read {} entries from {path}", entries.len());

    let mut brain = Brain::new();
    let mut pairs_seen = 0_usize;
    let mut compactions = 0_usize;
    let mut directives = 0_usize;
    let mut utility_calls = 0_usize;
    let mut threads: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Walk the JSONL as a single linear stream: every /v1/messages
    // request is observed against an empty response view (we don't
    // need the response body for rung 1; the brain only reads
    // request fields and response.usage). For convenience we synth
    // an empty response per request — this matches what a streaming
    // observer would do at request-receive time.
    for entry in &entries {
        if !entry.is_request() {
            continue;
        }
        let Some(url) = entry.url() else { continue };
        if !url.contains("/v1/messages") || url.contains("/v1/messages/count_tokens") {
            continue;
        }
        // Skip non-Anthropic /v1/messages-shaped paths just in case.
        if entry.provider() != Some("anthropic") {
            continue;
        }
        pairs_seen += 1;
        let pair = noodle_embellish_core::DecodedPair {
            request: entry.clone(),
            response: TapEntryView::from_value(serde_json::json!({
                "direction": "response",
                "event_id": entry.event_id().unwrap_or(""),
            })),
            events: Vec::new(),
        };
        // Non-chat paths (OAuth, MCP registry, etc.) lack a
        // `session_hash` — the brain returns None and we skip them.
        let Some(obs) = brain.observe(&pair) else {
            continue;
        };
        if obs.compaction_detected {
            compactions += 1;
        }
        if obs.compaction_directive_present {
            directives += 1;
        }
        if obs.thread_id == noodle_embellish_core::UTILITY_THREAD_ID {
            utility_calls += 1;
        }
        threads.insert(obs.thread_id.clone());
        println!(
            "evt={evt:<10} thread={tid:<28} turn={turn:<3} cm_dir={cmd} kind={kind:<28} \
             cm_det={cmd_det} dropped={dropped:<3} added={added:<3} beta={beta}",
            evt = entry.event_id().unwrap_or("?"),
            tid = obs.thread_id.chars().take(28).collect::<String>(),
            turn = obs.thread_turn_index,
            cmd = obs.compaction_directive_present,
            kind = obs.compaction_directive_kind.as_deref().unwrap_or("-"),
            cmd_det = obs.compaction_detected,
            dropped = obs.blocks_dropped,
            added = obs.blocks_added,
            beta = obs.api_context_management_beta,
        );
    }

    eprintln!("# summary");
    eprintln!("#   /v1/messages observed: {pairs_seen}");
    eprintln!(
        "#   distinct threads:      {} (incl. utility bucket)",
        threads.len()
    );
    eprintln!("#   compactions detected:  {compactions}");
    eprintln!("#   directives lifted:     {directives}");
    eprintln!("#   utility calls:         {utility_calls}");
}
