//! Replay a `tap.jsonl` file through the D2.2 Watchtower
//! [`ChainClassifier`] and print per-pair verdicts. The sanity check
//! that the bash-destructive rule fires on real captured Anthropic
//! `tool_use` blocks — and that safe commands fall through to
//! `AllowAllClassifier` — against the corpus the embellisher actually
//! consumes in production.
//!
//! Usage:
//!
//! ```sh
//! cargo run --example policy_replay -p noodle-embellish-core -- ~/.noodle/tap.jsonl
//! ```
//!
//! Unlike `brain_replay`, this example needs both the request AND
//! the decoded response events (the classifier reads
//! `DecodedEvent::ToolUse`), so it pairs records by `event_id` and
//! drives [`decode_pair`] over each pair before classifying.

use std::collections::HashMap;
use std::path::PathBuf;

use noodle_embellish_core::{
    ChainClassifier, PolicyClassifier, TapEntryView, decode_pair, read_tap_jsonl,
};

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

    let chain = ChainClassifier::d2_default();
    let mut pending_requests: HashMap<String, TapEntryView> = HashMap::new();
    let mut pending_responses: HashMap<String, TapEntryView> = HashMap::new();
    let mut pairs_seen = 0_usize;
    let mut flagged = 0_usize;
    let mut allowed = 0_usize;
    let mut rule_counts: HashMap<String, usize> = HashMap::new();

    for entry in entries {
        let Some(event_id) = entry.event_id().map(str::to_owned) else {
            continue;
        };
        let pair = if entry.is_request() {
            if let Some(resp) = pending_responses.remove(&event_id) {
                Some((entry, resp))
            } else {
                pending_requests.entry(event_id).or_insert(entry);
                None
            }
        } else if entry.is_response() {
            if let Some(req) = pending_requests.remove(&event_id) {
                Some((req, entry))
            } else {
                pending_responses.entry(event_id).or_insert(entry);
                None
            }
        } else {
            None
        };

        let Some((req, resp)) = pair else { continue };
        pairs_seen += 1;
        let evt = req.event_id().unwrap_or("?").to_owned();
        let url = req.url().unwrap_or("?").to_owned();
        let decoded = decode_pair(req, resp);
        let Some(verdict) = chain.classify(&decoded) else {
            continue;
        };

        *rule_counts.entry(verdict.rule.clone()).or_default() += 1;
        match verdict.decision {
            noodle_embellish_core::PolicyVerdict::Allow => allowed += 1,
            noodle_embellish_core::PolicyVerdict::Flag => {
                flagged += 1;
                println!(
                    "FLAG  evt={evt:<10} rule={rule:<22} risk={risk:.2} url={url}\n      → {rationale}",
                    rule = verdict.rule,
                    risk = verdict.risk,
                    rationale = verdict.rationale,
                );
            }
            _ => {}
        }
    }

    eprintln!("# summary");
    eprintln!("#   pairs scored:    {pairs_seen}");
    eprintln!("#   flagged:         {flagged}");
    eprintln!("#   allowed:         {allowed}");
    eprintln!("#   by rule:");
    let mut counts: Vec<_> = rule_counts.into_iter().collect();
    counts.sort_by_key(|c| std::cmp::Reverse(c.1));
    for (rule, n) in counts {
        eprintln!("#     {rule:<24} {n}");
    }
}
