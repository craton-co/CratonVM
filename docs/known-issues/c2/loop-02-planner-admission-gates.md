# LOOP-02 — the four whole-compile refusals, measured and then retired

**Status:** closed 2026-08-03. Three of the four refusals are gone; the fourth
is kept on measured grounds recorded below. **Owns:**
`jit/src/x64/loop_rewrite.rs`, plus the coordinate change in
`jit/src/x64/deopt_stubs.rs` and its check in `jit/src/x64/driver.rs`.

## The finding

`plan_bytecode_loop_xform` refused the whole compile, before it looked at a
single loop, when any of these held:

* `deopt_real` is enabled,
* precise exception frames are in use,
* the method contains `invokedynamic`,
* the method has inline sites.

Each refusal was defensible: they name constructs that publish an emitter pc to
the VM as a resume bci through a path the wiring did not translate.

## The measurement

`metrics::LOOP_XFORM_EVENTS` counts all four **independently**, on every compile
that reaches the planner, and is read at exit with `CRATONVM_DBG=jit-method-stats`.

Counting them independently is the whole point, and it is what closed this lane.
The planner returned on the first refusal that held, so a "which one fired" tally
would have recorded `deopt_real` for 100% of compiles and said nothing about the
other three — it is a process-wide default-ON flag, not a property of any method.
The four counts therefore **overlap** and must never be summed.

Spring Boot, `core/spring-boot-autoconfigure`, three test classes, one run each.
**Before** (2026-08-03, first increment):

| class | compiles | deopt_real | precise exc frames | invokedynamic | inline sites | **eligible** |
|---|---:|---:|---:|---:|---:|---:|
| `AutoConfigurationSorterTests` | 206 | **206 (100%)** | 0 | 3 (1.5%) | 8 (3.9%) | **0** |
| `ConditionalOnClassTests` | 137 | **137 (100%)** | 1 (0.7%) | 4 (2.9%) | 1 (0.7%) | **0** |
| `ConditionalOnPropertyTests` | 865 | **865 (100%)** | 7 (0.8%) | 65 (7.5%) | 34 (3.9%) | **0** |

**After**, same three classes, same configuration, same binary for both columns:

| class | compiles | deopt_real | precise exc frames | invokedynamic | inline sites | **eligible** |
|---|---:|---:|---:|---:|---:|---:|
| `AutoConfigurationSorterTests` | 228 | 228 (counted only) | 0 | 3 | 11 (4.8%) | **217 (95.2%)** |
| `ConditionalOnClassTests` | 168 | 168 (counted only) | 1 | 4 | 3 (1.8%) | **165 (98.2%)** |
| `ConditionalOnPropertyTests` | 958 | 958 (counted only) | 7 | 67 | 42 (4.4%) | **916 (95.6%)** |

The compile counts moved a little between the two measurements because `dev`
moved; the rows to read are `eligible`, which went from 0 to 95–98%.

## What retired three of them

One change: **`DeoptimizationPoint::bci` and `FrameState::bci` are published
through `Compiler::orig_bci`**, the rewrite's own provenance map, at the single
place they are built (`build_and_record_deopt_point`). It is the identity when
nothing was rewritten, so the default compile path is byte-identical —
`jit/tests/x64_artifact_corpus.rs` is the check that says so.

Everything else in that function keeps the emitter pc, because everything else
is an analysis **of the rewritten method**: `local_liveness`, the oop masks,
`local_kinds_refined`, `indy_stack_arg_types`, the scalar-replacement maps, the
callers' `*_box_ptr_by_bci` keys, and `emit_deopt_stubs`' stub sharing (which
must stay per-copy, or one copy's snapshot pointer is baked into another copy's
exit).

Resuming a copy at its original bci is exactly right: copy `j` of an unrolled
body IS iteration `i + j`, executing the same bytecode with the same abstract
frame, so the interpreter continuing at that bci continues the same computation.

Three things were needed that this doc's plan did not mention:

* **`pc_is_protected` asked its question with an output pc.** It range-tests
  against the method's un-rewritten interpreter exception table, and it is read
  by the sibling tail-call *regardless* of `precise_exception_frames` — so this
  was already wrong under a rewrite, before any gate was touched. It now
  translates. (`loop-rewriter-wiring.md`'s table census said `protected_ranges`
  "does not need" rewriting because its only consumer is on the
  `precise_exception_frames` paths. That was wrong; there are three consumers
  and two of them are not.)
* **The versioning guard's synthetic bytes are no longer OSR-eligible.**
  `LoopXform::osr_entry_pc` already refused an OSR *entry* there — that was the
  `probes/LoopVersionOsrProbe.java` wrong-code bug. But eligibility also gates
  the OSR-*exit* snapshot, which went live the moment the `deopt_real` refusal
  was retired, and an exit map on the guard would publish the header's bci with
  the guard's frame under it. `Compiler::synthetic_guard_span` is the one new
  piece of state.
* **The coordinate change is checked, not trusted.** `Compiler::deopt_point_pcs`
  keeps each point's emitter pc and
  `loop_rewrite::rewritten_deopt_points_are_publishable` re-derives the
  translation at finalize.

## The check, and what it refuses

Four conditions, all fail-closed on the METHOD (it stays interpreted, which
costs a compilation; publishing an output pc as a resume bci resumes arbitrary
bytecode):

1. every point still has its emitter pc (a `deopt_points` push that forgets
   `deopt_point_pcs` would misalign everything below);
2. the published bci is what the provenance map says, and is inside the original
   method — the translation itself, checked;
3. no point sits on the versioning guard (condition 2 cannot catch this: the
   guard's bytes DO answer `bci_at`, with the header's bci);
4. two copies of one `(bci, reason)` do not disagree about a field a bci-keyed
   consumer takes **on trust**.

Condition 4 is narrower than it first looks, and getting it wrong cost a real
method before the scope was right. The split is argued at
`loop_rewrite::PointDifference`:

* `semantics`, `action`, `speculation_id`, the frame bci, the method key and the
  absence of a caller scope are read and applied without being checked against
  anything. Disagreement there is fatal.
* the frame's SLOT KINDS, operand-stack depth and monitors are read only by
  `osr_entry_frame_state`, building the OSR *entry contract* — which
  `try_osr_entry` then verifies slot by slot against the live interpreter frame,
  refusing the entry on a mismatch. Picking the wrong copy there can only refuse
  (or accept) an entry the other copy would have decided the other way, and both
  outcomes are safe: seeding is by machine home, which is method-wide in this
  backend, and every path that RECONSTRUCTS a frame finds its point by native
  offset or through the box pointer the copy's own deopt stub bakes — never by
  bci. So a divergence is counted (`loop_xform_deopt_frames_diverge`) and logged
  under `CRATONVM_DBG=jit-gen`, not refused.
* a `FrameValue`'s machine LOCATION is not compared at all. Operand spill
  offsets are handed out as the walk emits, so copy 1's operand lives in a
  different frame slot from copy 0's, and both are right for their own copy.

`IndyDeoptProbe.concatLoop` is what taught this. Its two unrolled copies
disagree about local 3 at the `invokedynamic` — `Register(12)` vs
`RegisterRef(12)` — because the forward oop dataflow reaches copy 1 through copy
0's `astore_3` and reaches copy 0 through the loop entry, where the local is not
yet a reference. The first version of condition 4 compared `FrameValue`s and
discarded the method for it.

## The fourth gate, and why it stays

`InlineSitesPresent` still refuses, and this is a measured decision rather than
a deferral.

* It costs **1.8–4.8%** of compiles (11/228, 3/168, 42/959 above).
* Of the compiles it does not cost, the overwhelming majority are already
  refused for a reason that has nothing to do with admission: armed, on the same
  three classes, `loop_xform_no_candidate_loop` is 205/217, 150/165 and 849/917.
  Retiring the gate would move `loop_xform_applied` by a couple of methods.
* Retiring it is not a relaxation, it is a piece of work. `FrameState::caller` is
  `None` at every x64 site — this backend records no inline scopes at all — so
  the hazard is not an undescribed caller chain, it is that while the emitter
  walks an inlined callee's bytecode the pc it holds is a CALLEE bci, and
  `orig_bci` would translate it as if it were a caller output pc. Making that
  sound means teaching `x64/inlining.rs` to say which space the current pc is
  in, and then giving the callee's bcis a provenance map of their own
  (`docs/jit/deopt-inline-scopes.md`).

The doc's own step 3 said "and only if it is still measurably in the way". It is
measurably not.

## What is validated

* All three Spring Boot classes above pass **armed**
  (`CRATONVM_JIT=rootsnap-cache,bytecode-loop-xform`), 18/5/38 tests, 0 failed,
  **three runs each**, with 10, 15–16 and 58 methods rewritten per run and
  `loop_xform_deopt_bci_unpublishable = loop_xform_deopt_frames_diverge = 0`
  in all nine. `loop-rewriter-wiring.md` listed "no application suite has run
  with the rewriter armed" as a gap; that gap is closed for Spring Boot.

  Three runs rather than one because this is the arm where a wrong resume bci
  would show up, and a single green run of a suite that is green anyway says
  very little. The counters are stable to ±1 compile across the three.
* `probes/LoopXformProbe.java`, `probes/LoopVersionOsrProbe.java`,
  `probes/IndyDeoptProbe.java`, `bench/StringRegexOnly.java` and
  `bench/HashMapOnly.java` match HotSpot exactly in all three configurations —
  default, `bytecode-loop-xform`, and `bytecode-loop-xform,deopt-real=0`.
* `cargo test -p cratonvm-jit`: 1858 + 199 tests, all passing, including the
  artifact-corpus differ that pins the default path byte-for-byte.

## The doc's own guesses, scored

* It supposed "`invokedynamic` accounts for 95% of refusals on Spring code". It
  accounts for at most 7.5%.
* It proposed narrowing `InlineSitesPresent` FIRST, then corrected itself to
  last. Last was right, and "never, on these numbers" is where it landed.
* It said the remaining work was "translating `DeoptimizationPoint::bci` at
  `build_and_record_deopt_point`". That was the largest piece but not the whole
  of it — see the three additions above, two of which were latent bugs in the
  existing wiring rather than new work.

## Reading the tally

Twelve rows, `CRATONVM_DBG=jit-method-stats`, printed at exit next to the
tiering stats. The counters do not consult `metrics::enabled()`, so a default run
shows real numbers, and they are also in `metrics::summary()` next to the bailout
table (`MetricsSummary::loop_xform`, and the `"loop_xform"` object in its JSON).

| row | meaning |
|---|---|
| `loop_xform_compiles` | denominator: compiles that reached the planner |
| `loop_xform_deopt_real` … `loop_xform_inline_sites` | the four conditions, overlapping, one increment per compile each holds for. Only the last still refuses |
| `loop_xform_eligible` | no whole-compile refusal held — today `compiles - inline_sites` |
| `loop_xform_not_armed` | …and the rewriter was not armed. Equals `loop_xform_compiles` on a default run |
| `loop_xform_no_candidate_loop` | armed and eligible, but no loop passed the band and the structural test |
| `loop_xform_planner_refused` | armed and eligible, a loop was chosen, the rewriter refused it |
| `loop_xform_applied` | rewritten bytecode was compiled |
| `loop_xform_deopt_bci_unpublishable` | …and then DISCARDED by the check above. A defect, not a tuning signal: expected zero forever |
| `loop_xform_deopt_frames_diverge` | two copies' frame shapes differed. Reported, not refused — see above. Non-zero is normal |

The bottom five need `CRATONVM_JIT=bytecode-loop-xform` to be anything but zero;
see `docs/jit/loop-rewriter-wiring.md`. They no longer need `-deopt-real` as
well, which is the whole of what this lane changed.

## The trap in measuring this

Arming the rewriter **also disables the native byte-copy unroller** — the two
are exact complements. So any A/B that arms the rewriter is changing two things
at once, and "the code got longer/shorter" proves nothing about whether a
bytecode transform happened. That is why this is a counter and not an artifact
diff, and why `loop_xform_applied` is what the tests assert against.

## What to refuse

Do not relax the remaining gate without the translation it is protecting. It
names a real path where an emitter pc reaches the VM as a resume bci, and the
gate is the only thing standing between that and a resume at the wrong bytecode.
Relaxing it means implementing the translation for it, and proving the
translation with the same shape of test the three retired ones got — one
accessor, four baking sites, a whole-compile check that the published bcis are
all in interpreter space, and a workload that runs the result.

And do not relax it because the tally says it is cheap. The tally says which
gates *cost* something, not which are *safe* to remove; those are different
questions and only the second one is about soundness. What the tally is for here
is the opposite direction: it says this one is not even worth the work.
