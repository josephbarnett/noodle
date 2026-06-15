# 039 — Rip-cord: health degradation on sustained mint failure

**Status:** open
**Depends on:** 038
**Design refs:**
[`docs/adrs/034-enterprise-ca-and-external-signing.md`](../adrs/034-enterprise-ca-and-external-signing.md) §3,
[`docs/adrs/024-fail-open.md`](../adrs/024-fail-open.md)

---

## 1. Value delivered

When the external signing authority becomes unavailable, noodle
detects sustained mint failure and automatically transitions to
fail-open: the health probe goes unhealthy, the entry transport
stops claiming flows, and end-user traffic proceeds directly to
LLM providers without inspection. No human action required. When
the signer recovers, the proxy returns to claiming flows
automatically. This closes the rip-cord loop specified in ADR 034
§3.

## 2. Acceptance criteria

1. A `MintFailureCounter` in `noodle-proxy` tracks consecutive
   mint failures across all hosts (not per-host).
2. After `mint_failure_threshold` consecutive failures (default
   5, configurable), the health probe endpoint reports
   `unhealthy`. The threshold is reset to zero on the next
   successful mint.
3. The existing fail-open contract (ADR 024) engages: the entry
   transport's health probe transitions from healthy → unhealthy
   and stops routing flows to noodle. End-user TLS handshakes
   succeed directly against the upstream LLM provider.
4. Cached leaves continue serving while the signer is down.
   Hosts with valid cache entries are unaffected by signer
   failure until cache expiry or proxy restart.
5. Recovery is automatic: when `mint_leaf` next succeeds, the
   counter resets, health restores to healthy, the entry
   transport resumes claiming flows.
6. `health_degrade_on_mint = false` config opt-out preserves the
   ADR 034 §3.3 trade-off (cached hosts keep working; uncached
   hosts get 502s indefinitely; never fail open on signer).
7. Capture-driven fault-injection test: 5 consecutive
   `SignerUnavailable` from the stub Vault causes the health
   probe to flip; one subsequent successful mint flips it back.
8. The transition is logged as a single `AuditEvent`
   (`health_degraded` / `health_restored`) — not 5 noisy
   `mint_failed` events plus a separate state change.

## 3. Abstractions introduced or refined

- **New type:** `MintFailureCounter` (atomic counter +
  threshold + clock) in `noodle-proxy::health`.
- **Refined:** `HealthProbe` (already exists per ADR 024) takes
  a new input — the mint-failure signal — combined with existing
  liveness/readiness signals via boolean AND.
- **New `AuditKind` variants:** `HealthDegraded`,
  `HealthRestored` — used for the once-per-transition log line.

## 4. Patterns applied

- **State** — the health probe is a small state machine
  (`Healthy` ↔ `Degraded`) driven by counter transitions; the
  state machine is explicit, not implicit booleans.
- **Observer** — `CertMintService` callers notify the counter on
  success / failure; counter notifies the health probe on
  threshold crossings.

## 5. Test plan

- **Unit:** `MintFailureCounter` — 4 failures → still healthy; 5
  failures → unhealthy; 1 success → reset to healthy; opt-out
  flag suppresses the degradation transition.
- **Property:** for any interleaving of successes and failures,
  the counter never reports unhealthy with fewer than threshold
  consecutive failures.
- **Integration:** wiremock Vault returns 5 consecutive 503s;
  health endpoint flips unhealthy within one probe interval.
  Next 200 response flips it back.
- **End-to-end (manual or scripted runbook):** with the macOS
  entry transport running, kill the stub signer; confirm
  end-user traffic proceeds directly to the upstream after the
  fail-open transition. Confirm wire-record emission resumes
  when signer returns.

## 6. PR scope

One PR. ~250 LOC counter + health-probe integration + audit
events + tests. Reviewable in 30 minutes.

## 7. Out of scope

- Per-host failure budgets (treating failure for one upstream
  differently from another) → ADR 034 §3.3 explicitly rejects;
  signer availability is the concern, not host-specific issues.
- CA rotation handling → ADR 034 §9 deferred.
- End-user bypass toggle → ADR 034 §3.4 explicitly out of scope.
- Rip-cord levels 1 and 2 (PKI revocation and empty dispatch
  table) — these are IT operations, not engineering. Runbook
  entries, not stories.
