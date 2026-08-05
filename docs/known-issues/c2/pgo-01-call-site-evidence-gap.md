# PGO-01 — the inlining policy has no per-call-site evidence to read

**Status:** not started. **Owns:** `../../../vm/src/runtime/interpreter/invoke.rs`,
`../../../vm/src/runtime/interpreter.rs`, `../../../jit/src/profile.rs`.

## The finding this lane exists for

`MethodProfile::record_call_site` and `record_call_site_borrowed`
(`../../../jit/src/profile.rs`, around lines 488 and 1135) have **zero callers anywhere
in `../../../vm` or `../../../jit`**. Verified by grep, twice, in two different waves.

Consequences, all of which are true right now:

* `CallSiteEvidence::Direct` is unreachable outside tests.
* `MethodProfile::call_sites` is always empty in a real run.
* There is therefore **no per-call-site hotness evidence for `invokestatic` or
  `invokespecial` at all**. The receiver-type profile covers virtual and
  interface sites; the static and special sites have nothing.

So the inlining policy in `../../../jit/src/lib.rs` — `plan_inline`,
`classify_receiver_shape`, and every budget constant with a written rationale —
is reading a data source that a real run never populates for a whole class of
call sites. That policy is tested and correct; it is starved.

## What landed already, so this lane does not redo it

The profile's *integrity* was fixed on 2026-08-01
(`fix(jit): make the receiver/branch profile counters overflow-safe and
deterministic`). Four defects: a `u32` overflow at ~43M observations at one
call site that panicked in debug **inside a JIT profile read**, a hash-order
tie-break that let two compiles of one profile seed different inline caches, a
non-saturating counter that wrapped to zero and turned the majority receiver
into the rarest, and two branch predicates with the same overflow.

That work also disproved a report premise worth remembering here: a
monomorphic-looking site has **not** overflowed its type slots, because the
live receiver table is uncapped. Do not design around a truncation that only
exists in the unwired `pgo.rs` copy.

## The first increment

Record call-site evidence at the interpreter's dispatch sites, and only that:

1. Find the `invokestatic` / `invokespecial` dispatch points. Expect them to be
   in `invoke.rs`; expect the count to be larger than you think — a sibling
   lane's handover this wave undercounted a similar census by half and missed
   an entire file.
2. Record through the existing API. Do not add a second one.
3. State the concurrency contract explicitly. These counters are written by
   many real OS threads. Within one method the existing store is a
   point-in-time consistent image under that method's own mutex; across
   methods, nothing. Mark which of your uses are heuristic and which are
   correctness inputs — a profile read that races is fine as the former and
   fatal as the latter.

## How to verify

A test that runs a method with a known static-call shape and asserts the
evidence appears — and a negative control that it does **not** appear for a
shape that should not record. The counter-integrity tests added in the fix
above are the model, including the four-recorder concurrency test that has no
wall-clock bound.

## What to refuse

Do not enable any speculation on this evidence in this lane. That is `pgo-02`,
and it needs a guard/deopt pairing that does not exist yet. Recording data is
safe; acting on it is not, until `pgo-02` lands.
