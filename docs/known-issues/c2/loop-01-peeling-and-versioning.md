# LOOP-01 — peeling and guarded versioning

**Status:** both increments this doc planned landed 2026-08-03. **Owns:**
`jit/src/x64/licm.rs`, `jit/src/scev.rs`, the planner arms in `jit/src/x64.rs`.
**Still open:** unswitching, interchange and fusion are unbuilt, and *nothing in
this lane executes in production* — see [Reachability](#reachability), which is
`loop-02`'s lane and is the reason a fourth transform is not the next thing to
build.

## What is built

Three transforms now, all sharing one rewriter, one refusal set and one
provenance map (`docs/jit/loop-transforms.md`):

| Transform | Entry point | Reachable from the planner |
|---|---|---|
| unroll | `plan_loop_unroll` | yes (PGO arm and static band, unchanged) |
| peel | `plan_loop_peel` | **yes, new** — the bypassable-header arm |
| guarded versioning | `plan_loop_version` | **yes, new** — all three arms go through it |

### Peeling, and what it is for

`plan_loop_peel` was written and tested but unreachable: the planner only ever
called `plan_loop_unroll`. Making it reachable needed a case where peeling is
the *better* answer, not merely a legal one, and there is one already in the
backend.

A **bypassable** loop header — one an external branch can enter, as reported by
`find_bypassable_loop_headers` — makes every speculating transform in this
backend drop its pre-header: the aaload and arith LICM hoists, the FP hoists,
the speculative-BCE guards, matrix-dot and the bulk-byte loops all filter on
that set, because `pc_to_native[header]` points *past* the pre-header and an
external edge would skip it. The planner's answer used to be to skip the loop
entirely.

Peel(1) moves the problem: the external edge still lands on copy 0, which flows
into the copy below it, so the steady-state loop is entered only by
fall-through and its own back edge. The loop the emitter then finds has a single
entry and keeps its hoists. The rewriter already admitted this shape — an edge
that targets the **header** is not `ExternalEntry`, only one landing *below* it
is — so this is a planner arm, not a new transform.
`peeling_removes_the_preheader_bypass_from_the_steady_state_loop` pins the
property at every factor; `the_planner_peels_a_bypassable_header_instead_of_skipping_it`
pins the arm.

### Guarded versioning

`plan_loop_version(…, kind, guard)` emits a pre-header check and lays down two
images of the loop: `kind`'s transform on the guarded path, an untouched copy of
the original on the failing edge.

```text
    original            version(guard, unroll(k))
    ────────            ─────────────────────────
    H: body             G: <guard>   ──(fails)──┐
       goto H           F: body      (copy 0)   │
                           …                    │
                           body     (copy k)    │
                           goto F               │
                        B: body     ◀───────────┘
                           goto B
```

Nothing falls into `B` from above: the region always ends in an unconditional
`goto` (checked before anything is emitted), so the fallback is reachable only
through the guard's branch and its own back edge.

`encode_preheader_guard` turns a `scev::PreheaderGuard` into bytecode. It emits
`iload`/`iload_<n>`, one integer constant push and one `if_icmp*` — nothing
else — and that short list is what makes the guard's provenance sound rather
than being timidity:

* an `arraylength` or field term would **throw** at a PC whose provenance is the
  loop header, i.e. report a throw the original method does not have at that
  bci (`GuardNotEncodable`);
* a threshold past `sipush` would need `ldc`, hence a constant-pool entry this
  rewriter cannot mint — it rewrites bytes, it does not own the class
  (`GuardNotEncodable`);
* `StrideInRange` is two comparisons and would need two fallback edges
  (`GuardNotEncodable`);
* a term or threshold that decides the guard at compile time is a verdict, not
  an obligation (`GuardIsConstant`) — an always-true guard means the caller
  wants the plain transform, an always-false one means the fast version is dead.

`scev` specifies these checks in 64 bits so `base + addend` is never
materialised as a wrapping `int` add. The encoding never materialises it: the
addend folds into the compile-time threshold (`T = limit - addend`, in `i64`),
leaving one `int` compare against a value proved to be in `i32` range.

Four properties carry the soundness, each with a test:

| Property | Test |
|---|---|
| the guard writes nothing, cannot throw, cannot poll, is stack-balanced | `a_versioning_guard_writes_nothing_and_balances_the_stack` |
| its bytes carry the header's bci but are **not images** of it, so no side table is replicated onto synthetic bytecode | `a_side_table_entry_is_never_replicated_onto_the_guard` |
| OSR enters the fallback — except at the header, where it enters the guard and re-selects a version | `osr_into_a_versioned_loop_takes_the_guard_or_the_fallback` |
| both loops keep a back-edge poll, checked on the emitted bytes | `both_versions_keep_a_back_edge_poll` |

The first is what licenses the second: because nothing in the guard can throw,
allocate, call or deopt, none of the four sites that bake a bci into machine
code (`Compiler::orig_bci`'s callers) can ever name a guard PC, and no safepoint
map is recorded there — so giving those bytes the header's bci keeps
`provenance_is_total()` true without inventing a resume point.

The guard's *meaning* is the caller's business. For peel and unroll it is a
**profitability filter only**: both are legal at every trip count, because every
copy keeps the body's own exit branches. A future transform that is legal only
above a minimum must supply a guard sound for that claim; the rewriter emits
what it is given and proves only that the fast path is unreachable when the
guard fails.

### The planner

All three arms go through `plan_versioned`, which asks
`loop_analysis::analyze_counted_loop_at` + `CountedLoop::prove_trip_count_at_least`
for a `trip >= copies + 1` witness. `Guarded` ⇒ versioned artifact; `Static`
(the minimum is already known) and `Refused` ⇒ the plain transform, which is
byte-for-byte what the planner emitted before versioning existed. A versioning
refusal falls back the same way.

`analyze_counted_loop_at` is new. `analyze_counted_loop` takes the caller's
*assertion* about `LoopForm`, and `PreTested` is only true when the exit test is
the first thing the header does — otherwise the body runs before the test and
every derived trip count is one too many. The new entry point reports where the
test was found, so the planner **checks** the form it asserts instead of
guessing it, and returns no guard when the check fails.

What versioning buys, stated honestly: the duplicated copies are only reached
when the loop actually runs often enough to use them (the commonest loop in
Java, `for (i = 0; i < n; i++)` with a runtime `n`, has a compile-time
`trip.min` of **zero**, so without this the planner duplicates a body that may
never execute), and a loop that runs fewer times takes an untouched copy of
itself rather than entering a 4x-unrolled body. What it costs: one body of
code. Neither has been measured, and cannot be — see below.

## Reachability

**Read this before building a fourth transform.** The wired path
(`compile_with_param_slots` → `plan_bytecode_loop_xform`) is off by default but
no longer unreachable. Two things gated it, and one is gone:

* the rewriter is armed by a thread-local nothing in the VM sets
  (`set_bytecode_loop_rewriter_armed`) or by
  `CRATONVM_JIT=bytecode-loop-xform` — still the case, deliberately;
* even when armed, `plan_bytecode_loop_xform`'s first whole-compile refusal was
  `DeoptRealEnabled` and `crate::deopt_real_enabled()` defaults to **on** — so
  arming was not enough. `loop-02` retired that refusal (and
  `PreciseExceptionFrames` and `InvokedynamicPresent` with it) by publishing
  `DeoptimizationPoint::bci` through the rewrite's provenance map. Arming is now
  sufficient: 95–98% of Spring Boot compiles are eligible, where it was 0%.

So a fourth transform would run once armed. `InlineSitesPresent` is the one
whole-compile refusal left, and narrowing it means implementing the bci
translation it protects — not deleting the gate.
`the_wired_compile_path_reaches_a_loop_under_the_default_configuration` pins
the fact; it used to be the opposite assertion, and it was a comment in one
test that three other tests silently depended on.

Compiling planner output *as* a method is still the sharper instrument for a
pure-provenance question — it isolates the emitter from the planner — which is
why the test below does it that way.
`a_versioned_artifact_publishes_its_osr_entries_inside_the_fallback_copy`
does it the only way available: it compiles the planner's rewritten bytes *as*
the method and asserts that, mapped back into interpreter-bci space, a mid-body
bci's OSR entry lands inside the fallback copy — after the guard and all four
guarded bodies — while the header's lands before them. `pc_to_native` is
non-decreasing in pc, so that comparison is a statement about position.

## What executing it found

Adding `CRATONVM_JIT=bytecode-loop-xform` (which at the time also needed
`deopt-real=0` — see [Reachability](#reachability); it no longer does) made
this the first configuration in which any of these transforms runs. The first
real workload put through it threw `NullPointerException`, deterministically,
while every unit test passed.

`LoopXform::osr_entry_pc` answered the loop header's OSR entry with the
pre-header **guard**, reasoning that re-evaluating it there is exactly what a
fall-through entry does. True of the bytecode; false of the machine code. An OSR
entry is only valid at a pc whose compiled state the entry trampoline can
reconstruct from the interpreter frame, and the emitter publishes that state at
loop headers — the guard is in the prologue's straight-line code, where a local
can still live in a register the trampoline does not seed. The loop ran with a
null receiver.

Every bci in the region, header included, now enters the fallback copy, which is
a loop header. `probes/LoopVersionOsrProbe.java` is the reproducer and the
bisect: the failure needed a versioned artifact **and** the OSR door **and** a
second loop in the method, and each of those three is a separate method in the
probe that was correct on its own.

Two things this says beyond the bug itself. The transform lane's own acceptance
criterion — "prove it fired by something only a transformed artifact has" — is
necessary but not sufficient: the artifact was correct, its *entry contract* was
not. And a bytecode-level transform can be provably sound as a bytecode
rewrite (the step-sequence equivalence tests all passed) and still be wrong,
because the coordinate change it publishes is consumed by machine-level
machinery with preconditions the bytecode does not express.

## Premises that did not hold

Three, found while implementing this, in the spirit of the campaign's rule 1:

1. **The transform section header cited a test that was never written.**
   `peeled_loop_is_not_bypassable` did not exist. The property *is* pinned,
   under the name `peeling_removes_the_preheader_bypass_from_the_steady_state_loop`,
   and that test only covered `k = 1`; it now covers every factor. The citation
   is corrected.
2. **"Broadening the vectorization gate moves the 11-of-27 corpus number" was
   false.** The gate now asks `prove_trip_count_at_least` for a witness instead
   of refusing on a compile-time interval that is `[0, i32::MAX]` for every
   runtime-bounded loop — the change this doc deferred — and the corpus number
   did not move, because the corpus contained **no** runtime-trip-count loop at
   all. `TripCountTooSmall` was the one refusal class with no must-admit /
   must-refuse pair in it. The pair was added: the number is now **12 of 29**,
   and `VecRefusal::TripCountTooSmall` now means "no runtime check settles it"
   (a constant trip count below the lane count, a decreasing or non-unit-stride
   loop, a post-tested loop, an unbounded entry value). `vec_emit` already
   discharged `TripCountAtLeast` with `VecGuardValues::Term`; only the gate was
   missing.
3. **Two of the three wiring docs described the pre-wiring state.**
   `loop-transforms.md`'s "Wiring" section claimed steps 2 and 3 were not done;
   they landed some time ago. `loop-transform-wiring.md` is superseded wholesale
   by `loop-rewriter-wiring.md` and now says so at the top.

## What remains in this lane

Not built, and each is a lane rather than an increment:

* **Unswitching.** Versioning is its skeleton — two copies and a guard — but the
  guard is a loop-invariant *branch inside the body*, and each version must have
  that branch resolved, i.e. **deleted**. Every transform here so far is a
  faithful image of the region, which is what makes `bci_of`, `outputs_for_bci`
  and the step-sequence equivalence tests as simple as they are. Deleting an
  instruction from one version breaks that symmetry and needs its own argument
  about what the two versions' images mean.
* **Interchange.** Needs a dependence test over array subscripts across two loop
  levels. `scev` has the affine machinery (`IndexExpr`, `index_span`) and
  `vector_gate::dependence_between` has the distance test, but neither reasons
  about a *nest*, and the rewriter's region model is one loop.
* **Fusion.** Needs equal trip counts proved (which `prove_trip_count_at_least`
  cannot express — it is a lower bound, not an equality) plus a cross-loop
  dependence test.

`loop-02` used to sit before all of them, because none of this executed under a
default configuration. It closed on 2026-08-03.

## Running it

```bash
CRATONVM_JIT='bytecode-loop-xform' CRATONVM_DBG='jit-gen' \
  cratonvm --java-home <jdk> -cp probes LoopXformProbe
```

`[JIT_GEN] bytecode loop rewrite: kind=… versioned=… …` is one line per rewrite;
`bytecode loop rewrite refused: …` is one per refusal;
`bytecode loop rewrite DISCARDED: …` is one per artifact thrown away because its
deopt points could not be published in interpreter-bci space, and should never
appear. One token — the second one this used to need, `deopt-real=0`, now
measures the deopt-real-off configuration rather than the transform.
