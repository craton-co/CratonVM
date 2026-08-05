# COV-07 — retired 2026-08-04: `athrow` gets a real IR lowering

Branch `fix/c2-cov07-athrow-20260804`, merged to `dev` and pushed. Retires
[`docs/known-issues/c2/cov-07-athrow.md`](../known-issues/c2/cov-07-athrow.md).

**The lane proceeded — this was not "keep the refusal."** The brief hedged
that question deliberately (asked for the reasoning to be written down either
way); the investigation found the IR tier already had everything it needed to
answer it, mostly built by cov-05 and never spent.

## The question the brief actually asked

> Can the IR tier express a throw whose handler resolution happens in the
> interpreter, and can it do so without inventing a second, divergent answer
> to where an exception goes?

**Yes, and it already does — for every OTHER fallible operation.** Tracing
the single-pass backend's own `0xbf` arm
(`jit/src/x64/bytecode_walk.rs:3692-3724`) settles it: `athrow` does **not**
jump to an in-frame handler either. It calls `jit_throw_exception(exc_ptr,
bci)`, which stashes the exception (or, on a null reference, the JVMS NPE)
and unconditionally returns the `i64::MIN` deopt sentinel — the exact same
"stash + sentinel + epilogue" protocol `Op::Call`'s callee-exception path and
`Op::CheckCast`'s `ClassCastException` path already use
(`Lowerer::emit_call_return_check`). The comment on `jit_throw_exception`'s
call site says it outright: *"compiled code cannot branch to an in-method
handler."* Handler resolution — local or not — happens afterwards, in the
**interpreter**, via `route_jit_exception_through_method`
(`vm/src/runtime/interpreter/exception_dispatch.rs`), which builds a fresh
interpreter `Frame` and runs the handler bytecode
(`execute_prebuilt_frame`). That function does not care which backend
produced the sentinel it is draining.

So `athrow`'s single-pass lowering was never a *different* mechanism from
`checkcast`'s — cov-05's own doc comment hedged that ("this is **not** the
same machinery `athrow` needs") without checking, and the check settles it
the other way. The two differ in exactly one respect, and it makes `athrow`
**simpler**, not harder: `jit_throw_exception` takes the throw's own bci as
a plain argument, baked as a compile-time immediate — precise by
construction, no reason-9 frame or `precise_exception_frame_sites_supported`
machinery required. `checkcast`/a nested call's exceptional exit, by
contrast, only gets a precise bci when RBC.6 explicitly publishes one.

## What actually landed

* **`ir::Op::Throw`** (`jit/src/ir.rs`) — a new terminator, `[ctrl, mem, exc]`,
  modelled on `Op::Return` for control-flow purposes and on `Op::CheckCast`
  for its `MemAccess::Opaque` memory shape. `IrBuilder::build`'s `0xbf` arm
  pops the exception ref, builds the node, sets `graph.exit`, and marks the
  block dead — bytecode after an unconditional `athrow` is reachable only via
  a real branch target, exactly like the code after `ireturn`.
* **`ir_lower.rs`'s `lower_terminator`** gets an `Op::Throw` arm: load the
  exception ref, bake the athrow's own bci as an immediate, `CALL
  jit_throw_exception`, then unconditionally join the SAME `call_exc_patches`
  shared bail stub every other exceptional `Op::Call`/`Op::CheckCast` exit
  uses. No new stub, no CMP (the helper never returns anything but the
  sentinel) — reused infrastructure, not new machinery.
* **`ir_compatible`'s `scan.has_athrow` conjunct is gone.** The field itself
  is untouched (still recorded, still in the debug trace); nothing reads it
  as a refusal reason any more.
* **The `has_return`-only reachability check in `ir_verify.rs::check_control`
  was widened to `has_return || has_throw`.** This was a genuine, separate
  defect this lane's own test caught: a method that unconditionally throws
  (`void fail() { throw new X(); }`) builds zero `Op::Return` nodes, and the
  pre-existing check unconditionally required one live and reachable — it
  would have rejected every such method's IR graph, silently, the moment one
  was built. Caught by `athrow_no_longer_refuses_ir_admission`
  (`jit/tests/ir_vs_singlepass.rs`), not by inspection.
* **Escape analysis, DCE, the scheduler, and the loop-body dominance walk**
  all needed `Op::Throw` added alongside `Op::Return` — `is_control()`,
  `eliminate_dead_nodes`'s root seeding, `ir_schedule`'s terminator/successor
  detection, `loop_body`'s forward-reachable-set exclusion. Most of this was
  **already built and waiting**: `escape_analysis::Op::Throw` existed before
  this lane touched anything, with a fully worked-out escape rule ("a thrown
  reference escapes exactly like a returned one") and an explicit note at
  `jit/src/lib.rs:10281` naming exactly what wiring it up would need — see
  "What was already there" below.
* **`program_order_proves_dominance` needed NO change.** Its own doc comment
  already named the exact condition under which it would ("if [a throw ever
  branches to a lower-id load] this predicate must gain a `may_throw` term")
  — and this lane's design keeps that condition false by construction: a
  throw always leaves the frame, never branches to an in-graph handler.

## What was already there (and why this was smaller than the brief feared)

The brief's worst case — "the IR lowerer would have to model exception edges
into handler blocks" — was never going to be true, because `IrBuilder::build`
already skips handler bytecode outright (STUB-S8, landed before cov-05) on
the premise that a compiled frame never takes an exception edge. `athrow`
fits that premise exactly; it doesn't strain it.

What was more surprising: `jit/src/escape_analysis.rs` already had a
complete, dead `Op::Throw` variant — doc comment, escape rule
(`Op::Return | Op::Throw` at line 934), lock-refusal classification
(`MayThrowInGap`), `produces_no_reference` membership — all written and
waiting, unreachable only because `ir::Op` had no `Throw` to map from. And
`jit/src/lib.rs:10281-10287` carried an explicit note, addressed to "whoever
does that," naming the one thing to check before wiring the bridge
(`program_order_proves_dominance`'s dominance assumption) and the exact
reason it would need to change. Both were almost certainly left by the cov-05
session, generalizing ahead of its own scope rather than leaving a TODO
comment. This lane's escape-analysis work was closer to "connect the wire"
than "build the mechanism."

## Measurement

**46 `scan.has_athrow` refusal events / 38 distinct methods**, measured on
`ConditionalOnPropertyTests` alone (one of the three workloads the original
89-event survey figure was taken across), on `dev` at `7d61331bb0` — i.e.
**before** `cov-05`/`cov-06` are merged (both are unmerged sibling branches at
time of writing: `fix/c2-cov05-checkcast-instanceof-20260803` and
`fix/c2-cov06-array-allocation-20260804`). Per the standing re-ranking rule
this project's `cov-*` lanes have established repeatedly, the true count on a
tree with those two already landed will be **lower**, not higher — re-survey
after this merges alongside them, don't quote this number against the
original 89.

Reproduce: `CRATONVM_DBG=ir-compiles` against `SbRunner
org.springframework.boot.autoconfigure.condition.ConditionalOnPropertyTests`
(`core/spring-boot-autoconfigure` module — see
[`reference_springboot_suite_on_azure_linux`]), then:

```
grep -A1 'ir_compatible refused: scan.has_athrow' <log> | grep '\[ir\] admission' \
  | sed -E 's/^\[ir\] admission //; s/:.*//' | sort -u
```

## Where the 89 (now 46-on-this-workload) events come from

The brief's requirement 1 — group them, explicit-throw-in-application-logic
vs. rarely-taken-validation-branch — turned out not to gate the decision
(reusing existing infrastructure makes both shapes equally cheap to lower),
but the answer is: overwhelmingly the second shape. Sampled refusals are
dominated by precondition/validation guards with a single explicit `throw` on
an otherwise-unremarkable method: `org/springframework/util/Assert.state`,
`IndexedElementsBinder.isAllowRecursiveBinding`,
`ConfigurationPropertyName.buildToString`, several `Binder.bind*` methods,
`ConcurrentReferenceHashMap$Segment.clear/restructure`. A handful are
rethrow/wrap shapes (`AnnotationsScanner.processClass`,
`BindConverter.convert`). None of the sample is the
`checkcast`-needs-`cov-07` case cov-05's own hedge worried about — that
question is independently answered No by cov-05's own closeout
(`docs/internal/cov-05-checkcast-and-instanceof-RETIRED-*.md`).

## Verification

* **The conjunct count falls and admission rises, on the same corpus, both
  binaries run back to back** (`ConditionalOnPropertyTests`,
  `CRATONVM_DBG=ir-compiles`):

  | | baseline (`7d61331bb0`) | fix |
  |---|---:|---:|
  | `scan.has_athrow` refusals | 46 | **0** |
  | admitted to the optimizing pipeline | 817 | 827 (**+10**, not +46) |
  | optimizing backend produced a body | 756 | 761 (+5) |
  | Spring Boot tests | 38/38 | 38/38 |

  Admitted rose by 10, not 46 — the standard `cov-*` re-ranking effect this
  project's other lanes have all shown: most of the 46 were ALSO blocked by
  a downstream conjunct (overwhelmingly `!scan.indy_ops.is_empty()`, the same
  one that blocks every `JitLocalHandler.java` method below) that only
  becomes visible once `has_athrow` stops short-circuiting first. **Every one
  of the 46 has_athrow refusals is gone**; not all 46 methods reach the
  builder, and that is the expected, correct outcome, not a shortfall.
* `jit/tests/ir_vs_singlepass.rs`:
  * `athrow_no_longer_refuses_ir_admission` — a throw-only method (`aconst_
    null; athrow`, no exception table) compiles via the optimizing pipeline
    (`used_ir_backend == true`) and produces the identical raw `i64::MIN`
    sentinel both backends are expected to produce. This is also what caught
    the `check_control` reachability gap above: before that fix, this exact
    test's IR compile failed `ir_verify` with "graph has no live Op::Return
    terminator."
  * `athrow_never_taken_branch_matches_single_pass` — an `if (n>0) return n;
    throw null;` shape, exercised only on its non-throwing arm. Pins that
    ADMITTING an athrow doesn't perturb DCE/scheduling/escape-analysis on the
    surrounding method — the scheduler must place the `Op::If`'s two arms
    into independent blocks (one a `Return`, one a `Throw`), neither cross-
    contaminating the other.
* `vm/tests/jit_local_exception_handler_tests.rs`, run with
  `CRATONVM_JIT_FORCE_C2=1` — 16/16 pass. **Two fixtures in this file, and
  they split cleanly:**
  * `JitLocalHandler.java`'s methods (`catchReturnStep`, `multiCatchStep`,
    `nestedTryStep`, `rethrowStep`, `throwsInHandlerStep`) do **not**
    exercise this lane — every one is refused by the pre-existing, unrelated
    `!scan.indy_ops.is_empty()` conjunct before `has_athrow` would ever have
    mattered (confirmed via `CRATONVM_DBG=ir-compiles` per-test, in
    isolation — the interpreter constant-folds their string-concat message
    building through `invokedynamic`/`StringConcatFactory`). These tests
    passing proves the lane did not BREAK the single-pass fallback path for
    indy-bearing throwers; it is not evidence FOR the new lowering.
  * `JitPreciseHandlerFrame.java` (the RBC.6 fixture) is indy-free and **is**
    real coverage: `maybeThrow(Z)V`, `append(StringBuilder,I,Z)V`,
    `Thrower.apply(I)I`, and `loopCall(IZ)I` — four distinct athrow shapes
    (a bare conditional throw; one inside `StringBuilder` mutation; one
    inside an `invokeinterface`-dispatched implementation; one that is
    never actually taken) — are each confirmed `admitted to the optimizing
    pipeline` / `optimizing backend produced a body` under
    `CRATONVM_DBG=ir-compiles`. Their CALLERS (`plainStep`, `buildStep`,
    `scopeStep`, `loopStep`) split exactly on RBC.6 as designed: `plainStep`
    (params-only handler) is ALSO IR-compiled, so
    `test_compiled_callee_catches_its_own_athrow` validates an IR-compiled
    `athrow` propagating into an IR-compiled caller's own local handler,
    20000 iterations, 0 mismatches. `buildStep`/`scopeStep`/`loopStep` each
    need precise non-parameter locals and correctly stay on single-pass —
    which means `test_precise_handler_frame_catches_a_throw_at_the_end_of_
    its_try`, `test_precise_handler_frame_keeps_a_handler_only_local`, and
    `test_compiled_callee_handler_resume_keeps_the_loop_iterator` each
    validate an IR-compiled `athrow` throwing INTO a single-pass-compiled
    caller's precise-frame handler resume — the cross-tier interop case,
    20000 iterations each, 0 mismatches. This is the doc's requirement 2
    ("throw past its own handler") and the `finally`/precise-locals
    interaction together, for real.
* Bci parity — not independently re-derived, because `Op::Throw`'s lowering
  bakes the SAME `(exc_ptr, bci)` argument pair the single-pass `0xbf` arm
  bakes, via the SAME `helpers.throw_exception` function pointer
  (`jit_throw_exception`). There is one bci-computation site to get right
  (`node.bytecode_pc`, set from the builder's `Some(pc)`), not two competing
  ones to reconcile. The `JitPreciseHandlerFrame` cross-tier passes above are
  the practical proof: a wrong bci there fails closed (escapes past the
  handler or corrupts the resumed locals), and none did across 80000
  combined iterations.
* `cargo test -p cratonvm-jit --release`: 1878 lib tests + 140
  `ir_vs_singlepass` tests + the rest of the suite, 0 failed (one lib test,
  `test_ir_compatible_rejects_over_cap`, needed its own `has_athrow`
  assertion flipped — it was pinning the OLD blanket refusal and is now
  pinning the new admission instead).

## What this lane does NOT own (found in passing, not fixed)

**A latent gap in `ir_lower.rs`'s shared exceptional-exit stub, pre-dating
this lane and not athrow-specific.** The single-pass backend's own shared
exception-check stub (`emit_exception_check_stub`,
`jit/src/x64/deopt_stubs.rs:1053`) stamps `jit_set_throw_bci(<this method's
own throwing-instruction bci>)` on every exceptional exit — a CALL's
post-invoke check, an implicit NPE, etc. — precisely so
`route_jit_exception_through_method` can range-test the right bci against
THIS method's own exception table (not a stale bci a nested callee's own
`athrow` left behind). `ir_lower.rs`'s equivalent
(`emit_call_exc_stub`/`call_exc_patches`) does **not** do this stamping for
`Op::Call`/`Op::CheckCast`'s exceptional exits — only `Op::Throw`'s lowering
gets a correct bci, because `jit_throw_exception`'s own `bci` argument stamps
it directly, independent of the stub.

Effect: an IR-compiled method with a local, narrower-than-whole-method
catch-all (`finally`) wrapping a CALL or a `checkcast` — not an `athrow` —
whose `find_jit_exception_handler` needs a real bci to distinguish it from an
unrelated one, currently falls back to the "throw pc unknown" path. Typed
handlers still match (by exception class), so a normal `catch (X e)` is
unaffected; a `finally` is the one shape a pc-unknown search deliberately
skips (`find_jit_exception_handler`'s own comment: "Narrower catch-all
regions are skipped ... typed handlers still match"). This is the exact
class of bug `FinallyBalanceProbe.java` pinned for the single-pass backend
(`66548471f`/`369d9c2324`) — same failure mode, different tier, and it
predates this lane: any method with a local `finally` wrapping a call has
been reachable by the IR tier since STUB-S8 admitted exception-table methods
generally, well before cov-07 touched anything.

**Confirmed real (by inspection of `find_jit_exception_handler`'s own
pc-unknown-path comment; not independently reproduced with a failing test —
see the follow-up), not fixed here** — out of this lane's ownership
(`ir.rs`'s `has_athrow` conjunct and nothing else, per the brief), and fixing
it correctly means adding the SAME per-bci stub grouping the single-pass
backend's `emit_exception_check_stub` uses (one stub per distinct throw-site
bci, not one shared stub for the whole method) to `ir_lower.rs`, which is a
change to shared machinery `Op::Call`/`Op::CheckCast`/every future fallible
IR op depends on — a new lane's worth of work, not a hotfix folded into this
one. Flagged as a follow-up task (spawn_task `task_17edecd2`,
"Stamp jit_set_throw_bci in IR's shared exceptional-exit stub").

**CLOSED 2026-08-04**, branch `fix/hib-athrow-sneaky-throw-20260804`
(`a466f953a`), exactly as prescribed above: one stub per distinct throw-site
bci, each stamping `set_throw_bci`, with every exceptional-exit site routed
through a single `push_call_exc_patch` so none can reach the stub untagged.
It **was** independently reproduced first — `probes/IrFinallyBciProbe.java`
leaked 198 927 skipped `finally` bodies in 200 000 iterations on `dev`
(`41349f661`) with the caller confirmed IR-compiled, against 0 on HotSpot and
0 under `--nojit`; 0 after the fix. Pinned by
`vm/tests/jit_ir_exception_stub_throw_bci.rs`. See
`fixed-suite-bugs/hibernate/offsetdatetimetest-zoneddatetimetest-athrow-ir-sneaky-throw-swallowed-20260804-FIXED.md`,
"Defect 2".
