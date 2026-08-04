# PGO-02 — speculative inlining needs a guard and a deopt path that can express it

**Status:** RETIRED 2026-08-04. The first increment shipped 2026-08-03; every
residual is closed except a deopt-capable guard, which is blocked on
`deopt::FrameState::caller` being populated and is now refused mechanically
rather than by convention. Retirement record:
`../../../internal/pgo-02-guarded-inlining-RETIRED-20260804.md`. Living
document: `../../../feature-designs/profile-guided-inlining.md`. The brief
below is kept verbatim as the ORIGINAL ask — including the verification
requirements it set, all of which are now met.

**Original status:** not started. **Depends on:** `pgo-01` for static/special sites;
independent of it for virtual sites. **Owns:** `jit/src/lib.rs` (the
`plan_inline` region), `jit/src/pgo.rs`.

## Current state, verified

The policy half is **done and tested**: `plan_inline`,
`classify_receiver_shape`, `InlineRefusal` / `InlineVerdict` /
`InlineDependency` / `InlineDecisionTally`, and every budget constant as a
named `pub const` with a stated rationale — depth limit, cold and hot size
limits, expansion cost caps, recursion cut, minimum observations before
speculating, and the monomorphic / bimorphic / megamorphic share thresholds.

What is missing is the other half: nothing *acts* on a verdict speculatively,
because acting requires a guard plus a deoptimization path that can rebuild the
inlined frame chain.

## The blocker, stated precisely

Deopt metadata must reconstruct the **full virtual frame chain**: each inlined
frame with its own correct bci and its own correct method. The 2026-08-01 audit
(`docs/jit/deopt-metadata-audit.md`) found and fixed six unchecked invariants
in that metadata, three of them fail-open, including three scope-chain walkers
that **silently truncated** and returned a well-formed answer about a stack
that never existed — one of which told two compile-time admission gates that a
chain it had not finished walking was resumable.

Those are fixed. The lane still has to answer: for an inlined frame chain of
depth *k*, is the metadata **total**? Not "does it usually work" — is there an
input for which it cannot express the state? If yes, that input must be an
inlining refusal, not a best effort.

Second blocker, from the same audit: every `FrameState` the IR lowerer builds
hard-codes an empty monitor list. A `synchronized` block inside an inlined
callee therefore has no monitor state to rebuild. Precise resume is already
refused for monitor-bearing graphs; if this lane inlines across a monitor, it
must keep that refusal.

## The first increment

Take the narrowest speculation that is worth anything: a **monomorphic virtual
call** at a site with enough observations, guarded by an exact class-id check,
falling back to the existing dispatch on mismatch — not to a deopt. That
ordering matters: a guard that falls back to the interpreter is a correctness
question; a guard that falls back to the normal call is a performance question.
Do the performance one first.

Only after that, and only if the metadata is shown total, consider a guard that
deoptimizes.

## How to verify

* A guard that never fires must be byte-identical in behaviour to no inline.
* A guard that always fires must produce the same observable results as the
  un-inlined path — same exceptions, same stack traces, same `finally`
  execution. This VM has already shipped a JIT-compiled `finally` that was not
  run on three escape routes; inlining multiplies that surface.
* A megamorphic site must refuse, and there must be a test that a *truncated or
  saturated* profile reads as megamorphic rather than as its dominant type.

## What to refuse

Any inline whose deopt metadata cannot name every frame in the chain. Any
inline across a monitor while the frame states carry no monitor list. Any
speculation seeded from a profile read that is not point-in-time consistent.
