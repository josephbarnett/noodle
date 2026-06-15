# ADR 048 — design ↔ code gap review and remediation designs

Audit of [`048-inject-extract-llm-self-classification.md`](048-inject-extract-llm-self-classification.md)
against the code on `main` (2882985). Every verdict cites file:line;
every wire claim is verified against `captures/max/*.mitm`. §1–§4
are the audit; **§6 is the normative remediation design for each
gap** — contracts, algorithms, and flows at implementable fidelity.

**Headline:** item 0 (lineage foundation) is shipped and refined by
ADR 049; items 1–3 are partially shipped; items 4–7 are not
implemented; there are two live logic bugs (G1, G2) plus one
contract violation in the config wiring (G3) that the ADR's own
text warned against — and one of the ADR's load-bearing premises
(G0) is disproven by the team's own capture, which inverts two of
the "gaps" into ADR corrections.

---

## 0. G0 — The ADR's replay premise is false; the code is right (FOUNDATIONAL)

ADR 048 §5.2: *"the API is stateless: the modification we make to
the first round trip is replayed by the agent in every subsequent
round trip of the turn."* §3 repeats it: the continuation *"still
contains our injected directive in its history."*

**This is not how the wire works.** The proxy's mutation is applied
in flight, between client and API. The client never sees it; it
rebuilds `messages[]` on every round trip from its **own local
transcript** — which contains only what it sent and what it
received. Two facts close the loop:

1. The injected block never enters the client's transcript (the
   client's copy of its own message is unmodified).
2. The model's marker output doesn't either — the strip removes it
   from the client copy, the only copy the client appends.

So under once-per-turn injection, the **final** round trip — the
only one whose reply we harvest — would carry no directive at all.
Extraction would return nothing on every multi-round-trip turn.
Once-per-turn injection is not unimplemented; it is
**unimplementable**. The implementers proved this empirically and
recorded it in code:
`crates/noodle-adapters/src/transform/attribution_inject.rs:14-19` —
*"the steering slot is client-rebuilt and resent each turn and
never round-trips back to us (proven by the 9-turn capture)."*

**Consequences, resolved:**

| Was filed as | Resolution under G0 |
|---|---|
| G4 "per-turn injection gate (item 5) missing" | The Anthropic path's stateless every-RT idempotent injection is the **correct** design. ADR §5.2 + item 5 + Appendix A's gate are rewritten (§6.R4). Two real code bugs survive: the OpenAI-path injector gates once per **session** (`crates/noodle-adapters/src/injector.rs:76-77`) — markers cease after the first request — and the quota-probe skip is unimplemented on both paths. |
| G6 "extraction every-RT contradicts §6.1 terminal-only" | Every-reply marker emission + every-RT harvest is the **correct** design (it is also what §8's own directive text commands). The *per-turn* semantics live at rollup: the terminal round trip's harvested values win — exactly §7's existing merge rule. §6.1 and Appendix A's stop-reason gate are rewritten (§6.R4). The allow-list-vs-generic-namespace drift in G6 remains a real (low-priority) gap. |

Token-cost note: the injected bytes are identical on every round
trip, so the prompt-cache prefix stays stable; user-message
placements never touch the cached prefix at all.

---

## 1. Logic gaps — code behaves differently than the accepted design

### G1 — Lineage can be stolen by an interposed side-call (HIGH)

ADR 048 §4.3 warns, verbatim: *"title-generation requests … run
under a different system prompt and would masquerade as
sub-agents"* and prescribes the `tool_use → tool_result` chain as
the attribution signal — *"We use the first to detect, the second
to attribute."*

The implementation attributes on detection alone. The
pending-children stack pops on **any** `NewAgentRun` decision —
`crates/noodle-adapters/src/marking/anthropic.rs:211-217`:

```rust
let lineage = if matches!(agent_run_kind, AgentRunDecisionKind::NewAgentRun) {
    self.pending_children.get_mut(session_id).and_then(|mut stack| stack.pop())
} else { … };
```

There is no guard on what kind of request is popping: no model
check, no `max_tokens` check, no tool-chain confirmation. The
failure interleaving:

1. Parent's response emits `tool_use(Task)` → `ParentRunRef` pushed.
2. A haiku title-gen (or quota-probe, or compactor) request opens
   **before** the real sub-agent's first request. Its system hash
   (or `None`) is unseen → `NewAgentRun` → **it pops the stack**.
3. The title-gen round-trip is stamped with the sub-agent's
   lineage; the real sub-agent arrives to an empty stack and gets
   `lineage = None`.

The test suite covers only the empty-stack case
(`unrelated_new_agent_run_has_no_lineage_when_stack_empty`,
`crates/noodle-adapters/tests/adr_048_sub_agent_state.rs`); the
steal-when-stack-nonempty interleaving is untested. The
`quota-and-title.fixture.json` capture exists and could seed the
missing test.

**Fix direction:** gate the pop — a `NewAgentRun` only pops when
the request is plausibly the promised child (not a quota probe
`max_tokens==1`, not a known harness side-call), or defer
attribution to the `tool_result` match as §4.3 prescribed.
`PendingToolUses` (`crates/noodle-proxy/src/pending_tool_uses.rs`)
already tracks `tool_use_id → request_id` for S11 pairing but does
not feed lineage.

### G2 — `pause_turn` splits the turn (HIGH)

ADR 048 Appendix A (normative) defines the continuation set as
`{tool_use, pause_turn}`. The code maps `pause_turn` to
`Unknown`, which **closes** the turn:

- `StopReason::from_wire` has no `PauseTurn` arm —
  `crates/noodle-core/src/marking.rs:112-119`.
- `closes_turn()` returns `true` for `Unknown` —
  `marking.rs:103-108`.
- A test **pins the wrong behavior**:
  `assert_eq!(StopReason::from_wire("pause_turn"), StopReason::Unknown)`
  at `marking.rs:459`.

The domain layer already knows better:
`crates/noodle-domain/src/vendor/anthropic.rs:25-27` defines
`TAG_STOP_PAUSE_TURN` as a *"partial-turn checkpoint."* Anthropic
emits `pause_turn` on long-running server-tool turns (web search);
when it occurs, the turn is split in two, the second half mints a
fresh `turn_id`, and a turn-scoped injector (when item 5 lands)
would re-inject mid-turn.

**Fix direction:** add `StopReason::PauseTurn` with
`closes_turn() == false`; flip the test at `marking.rs:459`.

### G3 — Operator-authored directive text and placement are discarded (HIGH, contract)

§8's central contract: *"The injection text is appended verbatim;
the operator owns it"* and *"Editing the default tag set is
editing that TOML file — never a Rust array literal."*

The production wiring loads the TOML and then uses only two
fields of it — `crates/noodle-proxy/src/lib.rs:358-383`:

```rust
let Some(ie) = config.inject_extract.as_ref().filter(|ie| ie.enabled) else { … };
let tag_names = ie.declared_tag_names();
…
cfg.injectors.push(Arc::new(OpenAiAttributionInjector::with_default_directive(tag_names)));
```

`InjectionConfig.text` (the operator's verbatim directive) and
`as` (the placement) are parsed, validated
(`crates/noodle-core/src/config/inject_extract.rs`) — and never
consumed. The directive on the wire is regenerated in code from
the tag names. Observed live on the rancher-desktop gateway
(viewer capture, session `a808fc8b`): the injected block is the
hardcoded `DEFAULT_DIRECTIVE` single-tag text
(`crates/noodle-adapters/src/transform/attribution_inject.rs:33`)
landing in the **system** slot — not the TOML's six-tag
`user_prepend` text.

What survives the TOML: the tag-name allow-list (drives both the
strip scanner and the generated directive's tag set) and the
`enabled` gate. What's lost: verbatim text, per-tag VALUE
vocabularies, the `<system-reminder>` envelope, placement, and
multiple-injection support.

### G4 — Injection-frequency reconciliation → resolved by G0 (two residual bugs)

Superseded as a "gap": per G0, every-RT idempotent injection is
the required design and the ADR is corrected (§6.R4). Residual
**code** bugs:

- **G4a** — the OpenAI-path injector gates once per session
  (`crates/noodle-adapters/src/injector.rs:76-77`,
  `Session::directive_injected`), which under G0's wire facts means
  the directive vanishes from round trip 2 onward. Fix: same
  stateless idempotent replacement as the Anthropic path.
- **G4b** — quota-probe skip (`max_tokens == 1`) is unimplemented
  on both paths; probes get a useless directive injected. Design in
  §6.R3.

### G5 — Turn rollup and per-turn OTLP grain (items 6–7) not implemented (PLANNED, LARGE)

No `compose_turn` exists anywhere in `noodle-embellish` /
`noodle-embellish-core` (grep over both crates). The grain is one
row per round trip keyed by `event_id`
(`crates/noodle-embellish/src/sqlite.rs:422` `INSERT OR IGNORE …
event_id`), and the shipper emits one OTLP log/span per row
(`crates/noodle-shipper/src/mapping.rs:38 row_to_otlp_log`). No
finality detection, no merge rules, no `round_trip_count` /
`turn_duration_ms` / `directive_injected` columns in the
`ai_telemetry_v_0_0_2` DDL (`sqlite.rs:296-414`).

What §7 needs that *does* exist: `turn_id` / `agent_run_id` /
`parent_*` columns (ADR 048 item 0 PR-C3), `context_json` on the
row, and the `context.*` → `gen_ai.activity.*` mirror in
`mapping.rs`. The rollup itself — §7's entire mechanism and the
§364 consumer-contract change — is future work.

### G6 — Extraction frequency → resolved by G0; allow-list drift remains (LOW)

The every-RT harvest is correct per G0; §6.1's "terminal-only" is
rewritten (§6.R4). The residual drift: extraction is performed by
the `MarkerScanner` FSM constructed with an explicit tag-name
allow-list (`crates/noodle-core/src/marker.rs:112`
`MarkerScanner::new<I,S>(tag_names)`;
`crates/noodle-adapters/src/transform/marker_strip.rs:70`), while
§6.1/§8 promise a generic namespace harvest with *"no
declared-attribute allow-list."* The
`[inject_extract.extractions]` namespace/format section is parsed
(`inject_extract.rs:159-…`) but nothing consumes it. Disposition in
§6.R4: the allow-list is the safer strip posture (bounded tag
grammar); the ADR text changes, not the scanner.

### G7 — Never-empty backstop mechanism differs (LOW)

§6.3 case 2: markers-only content → *"release the original,
unstripped."* Implementation: substitute a **single-space
placeholder token** so the content block stays non-empty while the
marker stays hidden (`marker_strip.rs:135, 170-194`). Same goal,
arguably better behavior (marker never leaks), but the documented
safety posture and the shipped one are different mechanisms.

### G8 — `SessionState.parent_session_id` is dead state (LOW)

Declared at `crates/noodle-core/src/marking.rs:239` with a
"preserved across writes" doc comment; no code path ever writes
`Some(…)` into it. The shipped lineage carries the parent's
session id inside `ParentRunRef.session_id` instead. Remove the
field or document it as reserved.

---

## 2. Test-obligation scorecard (ADR 048 Appendix A)

| Obligation | Status | Evidence |
|---|---|---|
| 1 — Interleaved parent/sub-agent trace; parent turn not split; sub-agent attributed, not merged | **Covered** | `adr_048_sub_agent_state.rs` (8 tests over the `parent-task-subagent` fixture: parent turn/run survive the sub-agent's interleaved RTs; lineage matches the spawning `tool_use.id`) |
| 2 — Strip suite: split markers, markers-only, non-marker prefix verbatim | **Covered** (with G7 mechanism caveat) | `marker_strip.rs` byte-split matrix + placeholder/never-empty tests; `marker.rs` FSM tests |
| 3 — E2E: one OTLP record per turn, `context.*` + `gen_ai.activity.*` lit, no markers in rendered output | **Blocked** | depends on G5 (no per-turn record exists to assert on) |
| (implied by G1) — lineage steal under interposed side-call | **Missing** | `quota-and-title.fixture.json` exists as seed; no test interleaves it with a pending spawn |
| (implied by G2) — `pause_turn` continuation | **Anti-covered** | `marking.rs:459` pins the wrong mapping |

---

## 3. Document corrections needed in ADR 048

| # | Where | Correction |
|---|---|---|
| D1 | header | Status is `proposed`; item 0 + items 1–3 (partial) are shipped. Mark the shipped portions and cross-reference ADR 049, which **supersedes §4.3's described mechanism** (tool_result matching) with the pending-children stack actually built. |
| D2 | §5.1.1 | *"`user_prepend` … Current shipped choice"* is false on the wire: the live placement is the system slot via `AttributionInjector`, and `placement.rs` (item 4) does not exist. |
| D3 | §6.1 vs §8 | Resolve the final-reply-only vs every-reply contradiction (see G6). The implementation chose every-reply; if that stands, §6.1 and Appendix A's stop-reason gate text must change. |
| D4 | §11 file list | `crates/noodle-core/src/inject_extract.rs` → actual `crates/noodle-core/src/config/inject_extract.rs`. |
| D5 | §2 | `stop_reason` enumeration omits `stop_sequence` and `pause_turn`. |

---

## 4. Implementation-plan scorecard (§11)

| Item | Status |
|---|---|
| 0 — lineage foundation | **Shipped** (PRs #123–#127; refined by ADR 049 + #128/#131) — modulo G1, G2, G8 |
| 1 — TOML config loader | **Shipped** (`config_loader.rs`, embedded `default-noodle.toml` with all 6 tags) |
| 2 — directive renderer | **Closed by R3** — the operator's `text` is applied verbatim; `with_default_directive` and the engine-path `DEFAULT_DIRECTIVE` are deleted |
| 3 — generalize `MarkerStripTransform` | **Partial** — tag set is config-driven via `declared_tag_names()`; namespace/format from `[extractions]` unused (G6) |
| 4 — placement matrix | **Closed by R3** — `crates/noodle-adapters/src/transform/placement.rs` realizes all seven placements incl. the §5.1.2 tool_result rule |
| 5 — injection gate | **Closed by R3 under G0's corrected contract** — stateless every-RT injection with content idempotence + quota-probe skip (`max_tokens == 1`); the per-turn gate was unimplementable (G0) |
| 6 — turn rollup | **Missing** (G5) |
| 7 — OTLP grain change | **Missing** (G5) |
| 8 — six tags in default config | **Shipped and honored** — post-R3 the wire carries the embedded TOML's 6-tag `user_prepend` text |
| 9 — end-to-end validation | **Partial** — lineage e2e done (ADR 049 §11); inject/extract/rollup e2e blocked on 4–7 |

---

## 5. Recommended sequence

1. **R1** (`pause_turn`) — one enum arm + one test flip.
2. **R2** (lineage fingerprint match) — closes G1 with a
   wire-verified anchor; also retires ADR 049 §9.5's LIFO concern.
3. **R3** (config-honoring injector + placement realizer) — closes
   G3, G4a, G4b, item 4; unblocks the §5.1.1 A/B intent.
4. **R4** (ADR rewrite) — G0 premise correction + D1–D5, same PR
   series as R3 so doc and wire agree.
5. **R5** (turn rollup + grain) — its own epic; consumer-contract
   coordination required before the shipper flips.
6. **R6** (minor: G7 doc, G8 dead field) — ride along with R4.

---

## 6. Remediation designs (normative)

Each design follows the contract → algorithm → edge-cases → tests
discipline. File paths name the intended seam.

### R1 — `pause_turn` is a continuation (closes G2)

**Contract.** `pause_turn` is Anthropic's partial-turn checkpoint
(long-running server tools). It must bind to the same continuation
behavior as `tool_use`: same `turn_id`, same `agent_run_id`, no
re-mint, lineage preserved.

**Change.** `crates/noodle-core/src/marking.rs`:

```rust
pub enum StopReason {
    EndTurn, MaxTokens, ToolUse, StopSequence,
    /// `pause_turn` — partial-turn checkpoint (server tool in
    /// progress). The turn continues; boundary effect identical
    /// to `ToolUse`.
    PauseTurn,
    Unknown,
}
// closes_turn(): PauseTurn → false (join the ToolUse arm)
// from_wire(): "pause_turn" => Self::PauseTurn
```

The decision table in `anthropic.rs` needs no change — it matches
on `Some(StopReason::ToolUse)` for Continuation; widen the pattern
to `Some(StopReason::ToolUse | StopReason::PauseTurn)`
(`anthropic.rs:181`).

**Edge cases.** None novel: an *interrupted* paused turn (user
abandons; agent never resumes) is closed later by the same
"later-turn-observed" rule the rollup uses; until then the slot
holds an open turn — identical to an abandoned `tool_use`.

**Tests.** Flip `marking.rs:459` to assert `PauseTurn`; add a
decision-table row test (slot present + `last_stop = PauseTurn` ⇒
`Continuation`); add a synthesized SSE fixture round-trip (no real
capture carries `pause_turn` — note in the fixture README that it
is synthetic until a server-tool capture is acquired).

### R2 — Lineage attribution by prompt fingerprint (closes G1)

**Verified wire anchor** (mitmdump over
`captures/max/parent-task-subagent.mitm`): the spawning
`tool_use(Agent|Task).input.prompt` appears **byte-for-byte** as a
text block of the sub-agent's first user message — `RT2 block#3 ==
spawn toolu_012Y8jeMfYYbNWTHPS1Nujbw prompt (717 bytes)`, and the
block persists across the sub-agent's continuations (RT3–RT6). The
interposed side-call (RT7) and the parent's resume (RT8) do not
match. The spawn input schema is
`{description, prompt, subagent_type}`.

**Contract.** A pending child is popped **only by the request that
carries its prompt**. Side-calls (title-gen, quota probes,
compactor) can never steal a `ParentRunRef` because they never
carry the spawn prompt. The stack becomes a keyed set; LIFO
ordering stops being load-bearing — which also retires ADR 049
§9.5's concurrent-dispatch concern (parallel Tasks with distinct
prompts match independently; identical prompts are symmetric, first
match wins).

**Changes.**

1. `ParentRunRef` gains `child_prompt_hash: Option<SystemHash>` —
   hash of `input.prompt` (`crates/noodle-core/src/marking.rs`).
2. `MarkerDetector::on_response_tool_use` gains the prompt hash.
   The wirelog already consumes decoded
   `ContentBlock::ToolUse { input, .. }` at flow close; hash
   `input["prompt"]` when present
   (`crates/noodle-proxy/src/wirelog.rs`, the
   `tool_uses_in`-consuming block).
3. Request-side: `compute_canonical_system_hash` already parses the
   body; the same pass additionally collects
   `first_user_text_hashes: SmallVec<SystemHash>` — one hash per
   text block of the **first** user message
   (`crates/noodle-adapters/src/marking/anthropic.rs`).
4. Pop logic (`anthropic.rs:211-217`) becomes:

```text
on NewAgentRun(request):
  H ← first_user_text_hashes(request)
  stack ← pending_children[session]
  match ← first entry e in stack where e.child_prompt_hash ∈ H
  if match:  remove e from stack; lineage ← e
  else:      lineage ← None          # side-call or unmatched child:
                                     # never steal, degrade unlinked
```

5. Hygiene: cap the stack (8 entries, evict oldest with an audit);
   entries evict with the session (TTL). An entry whose child never
   arrives is dropped silently at session eviction.

**Fallback.** `input.prompt` absent or harness rewrites the prompt
en route ⇒ no fingerprint match ⇒ `lineage = None` — exactly ADR
048 §4.3's promised degradation ("correct-but-unlinked"), never a
mis-attribution. No heuristic side-call denylist is needed in the
match path.

**Complexity.** One hash per first-user text block per request
(blocks are already in memory from the system-hash parse); stack
scan is O(pending spawns) ≤ 8.

**Tests.** (a) Replay `quota-and-title.fixture.json` interleaved
with a pending spawn — assert the side-call gets `lineage = None`
and the true child still pops its entry (the missing
steal-interleaving test). (b) Existing 8
`adr_048_sub_agent_state.rs` tests extended: fixtures gain
`first_user_text_hashes` + spawn `prompt_hash` fields (regenerate
via `tools/extract_capture_fixture.py`). (c) Concurrent-spawn test:
two pushed entries, children arrive out of LIFO order, both
attribute correctly.

### R3 — Config-honoring injector with placement realizer (closes G3, G4a, G4b, item 4)

**Contract.** What reaches the wire is the operator's
`InjectionConfig.text` **verbatim**, placed per `as`, on **every
round trip** (G0), idempotently, except quota probes. The hardcoded
`DEFAULT_DIRECTIVE` and `with_default_directive` are deleted; the
embedded `default-noodle.toml` is the only default (it is always
present, so no code fallback exists).

**Component shape.** Injection moves off the
`NormalizedRequest`-steering-slot path onto the raw-body rewrite
seam Appendix A already specifies — decode only as far as the
`messages` array over `serde_json` `Value`/`RawValue`, mutate,
re-encode, fix `Content-Length`. This honors §5.3's
unknown-field-preservation requirement structurally instead of
trusting codec round-trip fidelity.

New file `crates/noodle-adapters/src/transform/placement.rs`:

```rust
pub enum Placement { System, Prompt, UserPrepend, UserAppend, UserNew, AssistantPrefill, Metadata }
// parse: "raw"→System, "user"→UserAppend, per §5.1.1 aliases

/// Apply `directive` to `body` at `placement`. Returns None when
/// the placement's structural precondition fails (forward
/// unchanged — fail-soft, §5.3).
pub fn apply(placement: Placement, body: &mut serde_json::Value, directive: &str) -> Option<()>
```

Placement algorithms (the two with preconditions):

```text
UserPrepend:
  m ← last message with role == "user"; none → None
  blocks ← normalize m.content to block array
  k ← length of the leading contiguous run of type=="tool_result"   # §5.1.2
  insert text block at index k

UserNew:
  last message role == "assistant" ? append new user msg : None     # preserve alternation
AssistantPrefill:
  last message role == "user" ? append assistant msg(directive) : None
```

**Gate** (entire injection decision, replacing item 5's per-turn
gate per G0):

```text
inject iff enabled
       ∧ provider cell matches (anthropic /v1/messages in v1)
       ∧ not quota probe (body.max_tokens == 1)                      # G4b
       ∧ body does not already carry the directive (shared matcher)  # idempotence
```

Note the probe check drops the ADR's `claude-haiku-*` model
conjunct: `max_tokens == 1` alone is sufficient and model-name
matching rots. The OpenAI path (G4a) adopts the same stateless
gate, deleting `Session::directive_injected`.

**Wiring** (`crates/noodle-proxy/src/lib.rs:358-383`): build one
injector per `[[inject_extract.injections]]` entry from
`(text, placement)`; `declared_tag_names()` keeps feeding the strip
scanner only. Audit per Appendix A (`InjectAudit{before_hash,
after_hash, placement}` on `roundtrips.jsonl`) — unchanged
contract, now actually populated with the placement used.

**Tests.** Placement matrix unit tests per variant incl. the
`tool_result`-leading rule (seed: the §5.1.2 400-error shape);
idempotence (apply twice ⇒ byte-identical); unknown-field
preservation (body with unmodeled fields round-trips
byte-identical outside the mutated message); probe-skip; e2e:
`exec claude` through the proxy asserting the wire body carries the
TOML text at the TOML placement (the assertion G3 failed —
this is the regression test for it).

### R4 — ADR 048 rewrite (closes G0 doc-side, D1–D5, G6, G7)

A single editing pass over ADR 048, present-tense, no narrative:

| Section | Rewrite to |
|---|---|
| §3, §5.2 | The injection is applied on **every** round trip, idempotently; the client rebuilds its history and never carries our mutation (cite the capture proof). Delete the "replayed by the agent" premise. Item 5 becomes the R3 gate. |
| §6.1, Appendix A extractor | Markers are emitted on every reply and harvested on every round trip; turn-level values resolve at rollup with terminal precedence (§7 merge rule). Drop the stop-reason gate. State the tag-name allow-list as the shipped harvest contract (G6) and why (bounded tag grammar = strip safety). |
| §6.3 | Markers-only backstop: single-space placeholder (G7's shipped mechanism), not release-verbatim; keep release-verbatim for the overflow and non-marker cases. |
| §4.3 | Replace the tool_result-matching description with the R2 fingerprint design; cross-reference ADR 049. |
| §5.1.1 | Placement table marks reality: shipped placement is whatever the TOML's `as` says, realized by `placement.rs` (post-R3). |
| header, §11 | Status reflects shipped items (0–3 with caveats); file-path correction (D4); add `pause_turn`/`stop_sequence` to §2 (D5, R1). |

### R5 — Turn rollup and per-turn OTLP grain (closes G5; the remaining epic)

§7 already designs the merge; what was left undesigned is the
**trigger, the storage shape, and the migration**. Those three:

**Storage.** A separate `ai_turns_v0_0_1` table keyed by `turn_id`,
not turn-rows in `ai_telemetry_v_0_0_2`. Reasons: the RT table's
`event_id` key and 70+ RT-grained columns don't fit a turn row
without nullable-column sprawl; and a separate table lets the
shipper cut over by changing its cursor's source table — one
config flag, trivially reversible during the consumer-contract
transition (§7's named coordination risk).

Columns: identity (`turn_id`, `session_id`, `agent_run_id`,
`parent_*` — carried from the RT rows), sums (`input_tokens`,
`output_tokens`, cache token fields, `cost`), first-RT constants
(`provider`, `model`, client identity), terminal-RT facts
(`stop_reason`, `status_code`, `last_request_id`), computed
(`round_trip_count`, `turn_duration_ms`, `directive_injected`,
`context_json`), bookkeeping (`composed_at`, `shipped_at`).

**Finality + composition algorithm** (in
`noodle-embellish::embellisher`, after each RT insert):

```text
on rt_inserted(rt):
  if rt.stop_reason ∈ {end_turn, max_tokens, stop_sequence}:
      compose(rt.turn_id)                       # terminal RT seen
  prev ← open turn for (rt.session_id, rt.agent_run_id)
  if prev ≠ rt.turn_id:                         # agent moved on —
      compose(prev)                             # earlier turn gets no more RTs

compose(turn_id):                               # idempotent
  rts ← SELECT * FROM ai_telemetry WHERE turn_id = ? ORDER BY timestamp
  row ← merge(rts)                              # §7 rules verbatim
  INSERT OR IGNORE INTO ai_turns ... (turn_id)  # replay-safe
```

No timers (§7's argument stands: next-turn arrival is a signal, a
clock is a guess). Restart-safe: a sweep at startup composes every
turn whose terminal RT is present but whose `ai_turns` row is
absent. `pause_turn` (R1) is naturally non-terminal here.

**Invariant tie-back:** `context_json` merge takes the terminal
RT's harvested values with precedence (G0: every RT carries
markers; the terminal one is authoritative) — the rule §7 already
states, now load-bearing.

**Migration.** Shipper config `grain = "round_trip" | "turn"`
(default `round_trip` until the OTLP consumer signs off — §7's
contract question, still open and still a blocker for the default
flip). Both grains carry the same attribute vocabulary;
`gen_ai.activity.*` mirrors unchanged.

**Tests.** Replay `parent-task-subagent.fixture.json` through
embellish: assert exactly N `ai_turns` rows (parent turns +
sub-agent turn), summed tokens equal the per-RT sums, terminal
stop reasons correct, sub-agent turn carries `parent_*`; restart
sweep test (kill between terminal RT and compose, restart,
compose happens); idempotent re-replay (row count stable). This is
test obligation 3's substrate — with R3 in place the e2e asserts
`context.*` + `gen_ai.activity.*` on the turn row.

### R3 scope note — claude.ai chat cell

Retiring the engine-path injector also retires the opportunistic
claude.ai chat-shape injection (style-prompt composition). v1
scope is the Anthropic Messages cell (ADR 048 Appendix A);
re-enabling claude.ai requires a placement realizer over the chat
body shape (`prompt` + `personalized_styles`). Tracked as a
follow-up; the e2e suite pins the cell as byte-faithful
pass-through until then.

### R6 — Minor

- **G7**: covered by R4's §6.3 rewrite (placeholder mechanism is
  the documented design).
- **G8**: delete `SessionState.parent_session_id`
  (`crates/noodle-core/src/marking.rs:239`) — `ParentRunRef.session_id`
  carries the value that is actually used. If a future cross-session
  parent appears (claude.ai → CLI handoff), it re-enters through
  `ParentRunRef`, not session state.
