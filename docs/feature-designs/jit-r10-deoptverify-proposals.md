# Directions worth pursuing after round 10 wave 6, lane `deoptverify`

Scope: `jit/src/deopt.rs`, `jit/src/ir.rs`, `jit/src/x64/driver.rs`,
`jit/src/x64/deopt_stubs.rs`. Everything here was derived by **reading**; this
lane was not permitted to build, test or probe, and nothing below claims a
measurement. Where a number would settle a question, the proposal says which
command produces it.

What already landed in this wave is not repeated here; see the FIXED blocks on
`r10-earelock-singlepass-deopt-metadata-is-never-verified-FIXED-20260922.md` and
`r10-earelock-has-elided-monitor-is-never-set-FIXED-20260922.md`.

---

## 1. Arm the oop-map agreement lane on the single-pass backend

**Why it is off.** The lane joins a deopt point to an oop map by **exact
`native_offset` equality**. On the single-pass backend those two numbers come from
different program points: a `DeoptimizationPoint::native_offset` is `buf.pos()` at
the guard that records it, and an `OopMapEntry::native_pc_offset` is set at the
safepoint that pushes it. Registering `cm.oop_maps` under those keys would
compare a deopt point against whichever map happened to land on the same byte, or
— far more often — against nothing. Either way the lane would not be checking
what it claims to check, so it was left disarmed rather than armed and wrong.

**Why it is worth arming.** This is the backend the lane was *written* for. Its
own doc says so: "a real check for any backend wired in later whose deopt map and
oop map are built from different sources — which `jit/src/x64.rs` is
(`local_oop_masks`/`stack_oop_marks` vs. the register allocator's live sets)". The
IR tier is sound here by construction (`plan_slots` keeps disjoint `Ref` and
`Prim` free lists); the single-pass backend is sound here by two independent
analyses agreeing, which is exactly the situation a cross-check earns its keep in.

**What it needs, in order.**

1. Settle the coordinate question: is `OopMapEntry::native_pc_offset` on this
   backend an offset from the same buffer base as `DeoptimizationPoint::
   native_offset`? (`driver.rs`'s `fully_oop_covered` block notes
   `emit_safepoint_map` currently pushes entries the relocation path matches by
   safepoint id, and `ir_lower` registers only entries with a non-zero
   `native_pc_offset` for precisely this reason.)
2. Decide the JOIN. Exact equality is the wrong relation even if the coordinates
   agree: the right question is "which oop map is in force at this deopt point",
   i.e. the nearest preceding safepoint map, not the one at the same byte. That is
   a change to `DeoptVerifier` (a `with_oop_map_range` or a lookup by
   greatest-lower-bound), and it should be made in `deopt.rs` where the sign
   convention is already documented in one place.
3. Register `registers:` honestly. `OopCoverage::registers` is empty for the IR
   lowerer "which keeps every live value in a frame slot". The single-pass backend
   publishes `FrameValue::RegisterRef`, so the register half of the join is live
   here and must be fed from whatever the emitter marked.

**Expected finding, stated in advance so the result is falsifiable.** If this lane
is armed correctly, the shape it should catch first is a `RegisterRef` at a deopt
point whose safepoint map lists no registers — a reference the GC will not update
across a moving young collection, reconstructed from a stale word. If it reports
nothing, that is a real (and welcome) result about `stack_oop_marks` agreeing with
the allocator's live sets.

## 2. Settle the duplicate-`native_offset` question, then tighten the sortedness lane

**Half-answered since, and the answer went the other way — read this before
acting on the section below.**

* Wave 7 (lane `interner`) derived from the producer that duplicates are
  **possible**: three recorder families funnel into
  `build_and_record_deopt_point`, which emits no machine code, and they
  de-duplicate in three maps that never consult each other, so two adjacent
  metadata-only records at one pc land on one offset. The concrete adjacency is
  in `x64/bytecode_walk.rs` and is behind `CRATONVM_DEOPT_EAGER`, not the default
  configuration. Wave 8 (lane `offsetkey`) then showed such a pair necessarily
  shares its `bci` too, which makes the conclusion stronger.
* **So the lane stays `>` and tightening it is refused, not deferred.** It would
  refuse an artifact in a probe arm to prove a hypothesis, and the verifier
  cannot tell a legitimate pair from a mistaken one. The decision is in
  `DeoptMetadataError::DeoptPointsUnsorted`'s own doc and is pinned by
  `jit/tests/r10_interner_deopt_point_keys.rs::the_sortedness_lane_still_tolerates_the_equal_pair`,
  which fails on a one-character reversal.
* The `debug_assert` this section pairs the lane with no longer exists: wave 9
  deleted `CompiledMethod::find_deopt_point` outright (no production caller, and
  the ordering it needed is checked by this lane on the install path in release
  builds, which the `debug_assert` never was).
* What remains genuinely open is only the MEASUREMENT below — whether the
  constructible pair is ever actually constructed. It is worth taking, and it is
  no longer a prerequisite for anything: a duplicate found would confirm the
  decision rather than change it, and a duplicate not found would not license
  tightening, because absence over one workload is not the invariant.

The original text follows, with the strikethrough above applying to its
conclusion.

`DeoptVerifier::violations` tests `w[0].native_offset > w[1].native_offset`
strictly, and `CompiledMethod::find_deopt_point`'s `debug_assert` used `<=`. Both
therefore permit two points at one offset, and an exact-offset binary search over
such a list returns an arbitrary one of them — a resume at one of two bcis.

This lane did not tighten it, because doing so would refuse artifacts on the
strength of a guess. The question is narrow and mechanically answerable:

```text
# add a temporary debug print, or assert in a scratch build:
#   points.windows(2).any(|w| w[0].native_offset == w[1].native_offset)
# over a broad workload (CratonBench, H2, Spring) with the JIT on
```

If duplicates never occur, change `>` to `>=` (and the `debug_assert` to `<`) and
the ambiguity is gone by construction. If they do occur, the fix is at the
producer: two points at one byte means two snapshot sites recorded with nothing
emitted between them, and one of them is redundant.

See `r10-deoptverify-find-deopt-point-has-no-production-caller-RETIRED-20260922.md` for
the rest of that function's story.

## 3. Make `SlotRef` cheap enough that the verifier is free on the clean path

`DeoptVerifier::check_scope` builds a `SlotRef` per slot — `base(SlotKind::Local, i)`
— and each one clones `state.method_key`. `check_value` then clones it again into
`ScopeCheck::word_types` for every distinct frame word. So a clean verification of
a method with 20 points × 30 slots costs on the order of a thousand `String`
allocations, all of them thrown away.

That cost was already being paid on every optimizing compile; wiring the
single-pass backend doubles the number of compiles that pay it. It is compile-time
only and small next to a register allocation, so this is a cleanup, not a
regression — but it is the kind of cleanup that makes the "run it
unconditionally" argument unconditional.

Two shapes, in increasing order of churn:

* **Lazy `SlotRef`.** Pass `check_value` a closure (`&dyn Fn() -> SlotRef`) and
  build the `SlotRef` only when a violation is pushed. `word_types` still needs a
  cheap key for the conflict report; store `(SlotKind, usize)` and reconstruct.
* **`Arc<str>` method keys.** `SlotRef::method_key` and `MethodFrameLimits::
  method_key` become `Arc<str>`, cloned by refcount. This is a public-type change
  and would touch `DeoptMetadataError`'s Display, so it wants to ride with
  whatever else changes those types.

Neither is worth doing blind. The measurement that justifies it is compile time on
a method with many deopt points (`jit-method-stats` already reports per-phase
timings).

## 4. Give the single-pass backend a `MethodFrameLimits` for the callee's `max_stack`

The new verification registers real `callee_code_len` and `callee_max_locals` for
every `InlineSite`, and saturates `max_stack` to `u16::MAX` because no callee
`max_stack` reaches `compile_with_param_slots`. `InlineSite` already carries
`callee_code`, `callee_code_len`, `callee_max_locals` and `callee_num_args`; adding
`callee_max_stack` is a one-field change at the producer (`jit/src/lib.rs`'s
inline-site resolver, which read it from the callee's `Code` attribute to decide
admission in the first place).

It is only worth it in combination with the producer fix in
`r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`:
until a splice publishes a point, the limit has nothing to check.

## 5. `DeoptVerifier::requiring_oop_map` has no production caller

Both backends leave it at its default `false`, and `ir_lower`'s comment explains
why ("leave `requiring_oop_map(false)`, which is the documented default for a
backend whose maps are conservative"). It is a builder method whose only callers
are `deopt.rs`'s own tests.

It is not debt in the same sense as the interner — it is one line, its default is
the right default, and the day a backend anchors complete maps it is exactly the
switch that arms the `MissingOopMap` check. The proposal is only to say so in its
doc: "No production caller today; both backends' maps are conservative. Turn this
on for a backend whose maps are complete and anchored, together with proposal 1
in `jit-r10-deoptverify-proposals.md`." A one-line doc edit that stops the next
reader from having to run the grep this lane ran.

## 6. Publish `Unsupported` from inside a splice, as the interim honest frame

Restated from
`r10-deoptverify-splice-published-point-would-carry-the-callers-locals-FIXED-20260922.md`
because it is the cheapest correctness win on this list.

`build_frame_state_at` fills `self.num_locals` entries from the ROOT method's
local homes whatever identity it stamps. Inside a splice that is the caller's
locals under the callee's name. Until the geometry is parameterised, the honest
encoding for a callee-identified frame is `FrameValue::Unsupported` for every local
and stack slot: well-formed, accepted by every verifier lane, and fail-closed (the
frame is unresumable, so the method takes the safe re-run).

That is strictly better than today's two outcomes — the splice being rolled back
(losing the inline) or, once the VM's sink learns to read caller chains, a resume
from the wrong locals.

## 7. The verifier is now the only release-build check on this metadata — keep it that way

Worth writing down because it is a new property of the tree as of this wave, and
it changes what a future edit is allowed to do.

Before this change, the single-pass backend's deopt metadata was checked by
nothing; after it, the *only* release-build check is
`DeoptVerifier`. The `debug_assert` in `find_deopt_point` does not run in release,
`rewritten_deopt_points_are_publishable` covers only the loop-rewrite case, and
`baked_point_table_is` covers only buffer identity.

So two rules follow, and they belong in review rather than in a comment:

* a new `DeoptMetadataError` variant must be *reachable from a producer*, or it is
  the round-10 orphan shape in its most dangerous dress — a check that reads
  "clean" forever;
* a change that weakens a lane must say which producer it is weakening it for,
  because the refusal it removes is now the last one.
