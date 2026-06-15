# NNN — short title

**Status:** open · in flight · done
**Depends on:** NNN, NNN (other story numbers, if any)
**Design refs:**
[`docs/adrs/NNN-related.md`](../adrs/NNN-related.md),
[`docs/adrs/NNN-other.md`](../adrs/NNN-other.md)

---

## 1. Value delivered

One paragraph. What can a user or operator do after this story
ships that they could not do before? Frame from the demonstrable-
value lens, not the implementation lens.

## 2. Acceptance criteria

Concrete, pass/fail conditions. A reviewer (human or agent) must
be able to read each line and decide "yes that's met" or "no it
isn't" without ambiguity. Numbered list.

1. ...
2. ...
3. ...

## 3. Abstractions introduced or refined

Name the trait(s), struct(s), or module boundary this story
introduces or sharpens. Call out the seam where dependency
injection happens — what gets injected, who instantiates it, how
tests substitute a fake. If no new abstraction is introduced
(e.g. pure bug fix), say so explicitly.

## 4. Patterns applied

Gang-of-Four (or equivalent) pattern names where they earn their
keep. Examples: *Strategy*, *Decorator*, *Chain of
Responsibility*, *Observer*, *Template Method*, *Builder*,
*Composite*, *Adapter*, *Mediator*, *Command*, *State*,
*Interpreter*. Don't pattern-fish — only name a pattern when it
clarifies the design. If none apply, omit the section.

## 5. Test plan

What tests prove the acceptance criteria? Lean toward unit tests
with fakes against the injected trait. Property tests where the
invariant is structural. Integration tests only at the system
boundary or for end-to-end flow validation. Each acceptance
criterion should map to at least one test.

- Unit: ...
- Property (if applicable): ...
- Integration: ...

## 6. PR scope

What fits in **one PR**? If the story needs more than one PR,
split it into sub-stories (`NNN.a`, `NNN.b`, …) before starting.
Estimate lines of code if useful, but the binding constraint is
reviewability — a PR a reviewer can read in 30 minutes is the
target.

## 7. Out of scope

Explicit list of what this story does *not* try to do. Names the
adjacent work (typically other story numbers) where each excluded
item lives. Prevents scope creep and helps future readers see the
seam.
