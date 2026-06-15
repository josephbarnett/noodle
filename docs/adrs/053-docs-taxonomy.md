# ADR 053 — Documentation taxonomy: ADRs vs architecture vs knowledge vs guides

**Status:** Accepted — implemented by the PR that introduces this ADR.

**Supersedes** the ad-hoc `docs/` layout that had accreted five overlapping
top-level directories (`design/`, `decisions/`, `operations/`,
`kb-articles/`, `agent-protocols/`) with no agreed meaning and number
collisions across the series.

---

## 1. Why this exists

`docs/` had drifted into a layout where category and intent no longer
matched. The symptoms:

- **`design/` was already the ADR series** (`000`–`052`, with a status
  ledger in `000-tracking.md`) but was named "design," so nobody could
  tell whether a new doc was a decision record or a design sketch.
- **`decisions/` existed separately** — a name that *should* mean ADRs —
  but held a draft architecture narrative and a refactor plan, neither
  in ADR format. Two homes competed for the same concept.
- **No home for an as-built architecture narrative.** The present-tense
  "how the system is wired" doc (`decisions/architecture.md`) had no
  category that fit; it sat under `decisions/` by default.
- **`operations/` vs how-to guides** — the directory was all how-tos and
  runbooks; "operations" was the boring-but-vague label.
- **`kb-articles/` and `agent-protocols/`** were unsanctioned top-level
  dirs holding external-fact reference material and a coverage roadmap.
- **Junk in the tree:** `crap.json`, `thoughts.md`, `quic.log`, `.DS_Store`.

The root cause: the layout was organized by accident of history, not by
**what a document is for**.

## 2. Decision

Organize `docs/` by document *kind* — the question the doc answers —
not by topic. Six buckets:

| Bucket | Answers | Format / durability |
|---|---|---|
| `docs/adrs/` | "what did we decide, and why" | numbered `NNN-title.md`, point-in-time, immutable once landed |
| `docs/architecture/` | "how is the system wired, as it is now" | present-tense narrative, rewritten as the system changes; **not** ADR format |
| `docs/knowledge/` | "how does the external world we observe work" | reference; facts about systems we do **not** own (provider protocols, etc.) |
| `docs/guides/` | "how do I operate or do this" | runbooks, how-tos |
| `docs/features/` | "what will we build, in what order" | numbered story backlog; ships → `done/` |
| `docs/diagrams/` | the visuals | mermaid markdown, drawio |

Plus `docs/images/` as a pure asset directory.

### The ADR vs architecture distinction (the load-bearing one)

- An **ADR** is a *decision*: "we chose X over Y, on this date, because Z."
  Historical. Immutable once landed. Superseded, never edited in place.
- **Architecture** is a *description*: the system as it is now, present
  tense, no "we decided" framing. Rewritten as reality changes.

Mixing them is what produced the `design/` confusion. They are different
document kinds and live in different directories.

### Rules

1. **No `notes/`, no `slides/`, no junk.** A working note has exactly two
   fates: **promote** (graduate into an ADR, a knowledge article, a guide,
   or a feature) or **delete**. It never gets a standing home. `slides/`
   was a vestigial slot inherited from the portable global rule; noodle has
   no presentation decks, so it is dropped.
2. **ADRs and architecture do not merge.** Decisions are immutable history;
   architecture is living description.
3. **Knowledge is owned-by-nobody facts.** If we decided it, it's an ADR;
   if we built it, it's architecture; if we merely *observe* it, it's
   knowledge.

## 3. Migration performed by this PR

| From | To |
|---|---|
| `docs/design/` | `docs/adrs/` (rename; the series was already ADRs) |
| `docs/operations/` | `docs/guides/` |
| `docs/kb-articles/` | `docs/knowledge/` |
| `docs/decisions/architecture.md` | `docs/architecture/architecture.md` (its real home) |
| `docs/decisions/refactor-plan.md` | `docs/features/refactor-plan.md` (planning) |
| `docs/agent-protocols/coverage-roadmap.md` | `docs/features/agent-protocol-coverage-roadmap.md` (self-declared planning material) |
| `docs/decisions/`, `docs/agent-protocols/` | removed (now empty) |
| `crap.json`, `thoughts.md`, `quic.log`, `.DS_Store` | deleted (junk) |

All cross-references in docs **and code** (≈85 doc files + 36 source/config
files carrying doc-pointer comments) were rewritten. The sweep was anchored
to our own paths: the external `telemetry-backend/feature-ai-collector-macos/docs/design/`
string in `noodle-embellish` tests was deliberately **not** rewritten — that
points at a different repository.

`docs/architecture/architecture.md` predates the numbered series and has
v1-era drift (port numbers, crate split, header names). It was moved and
its dead "refactor pending" banner dropped, but its body was **not**
rewritten to match current code — a freshness pass against the current
crates is tracked as follow-up rather than fabricated here.

## 4. Consequences

- **Positive:** a new doc has one obvious home, decided by what it is for.
  The ADR-vs-architecture split ends the `design/`-means-what ambiguity.
- **Positive:** AGENTS.md's directory table and the global documentation
  rule now match the tree.
- **Cost:** a large one-time diff touching 36 source/config files (comment
  and string updates only — no behavior change).
- **Open follow-up:** the architecture narrative needs a freshness pass;
  `agent-protocol-coverage-roadmap.md` mixes protocol facts (knowledge) with
  a priority roadmap (planning) and may later split if the fact tables
  outgrow the roadmap. The root-level `doc-gaps.md` / `doc-gaps-status.md`
  analysis snapshots describe the pre-migration state and are now stale.

## 5. Security considerations

No attack surface, data, or permission change — this ADR moves and renames
documentation only. The one safety concern was the reference sweep
corrupting an unrelated repository's path: mitigated by anchoring every
`docs/design/` → `docs/adrs/` substitution to skip lines referencing the
external `telemetry-backend` checkout. No secrets live in `docs/`; the deleted junk
files (`crap.json`, `thoughts.md`, `quic.log`) were verified to contain none.
