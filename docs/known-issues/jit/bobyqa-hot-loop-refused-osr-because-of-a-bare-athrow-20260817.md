# `BOBYQAOptimizerTest` effectively hangs — its hot loop is refused OSR by RBC.6 (bare `athrow`, no local handler), the same structural gap as RBC.6b

**Status: OPEN, reproduced and diagnosed 2026-08-17, not fixed.**

Found triaging the Apache Commons Math test suite
(`apps/commons-math/RESULTS-20260817.md`): every `BOBYQAOptimizerTest` method
that actually runs the optimizer (`testConstrainedRosen` and siblings) times
out — no crash, no error, the process is genuinely still running and making
forward progress, just far too slowly to finish inside any reasonable test
timeout (a 90s per-class run-suite timeout, and it doesn't finish inside 15s
in isolation either, see reproduction below).

## Diagnosis

`--stack-dump-on-timeout 20` shows the interpreter is legitimately executing,
not deadlocked — three dumps 10s apart all show the same call stack shape,
deep inside `BOBYQAOptimizer.trsbox` (the trust-region subproblem solver),
called from `bobyqb`, called from `bobyqa`, called once per JUnit test:

```
BOBYQAOptimizerTest.testConstrainedRosen -> ... -> BOBYQAOptimizer.bobyqa
  -> BOBYQAOptimizer.bobyqb -> BOBYQAOptimizer.trsbox -> ArrayRealVector.setEntry
```

`CRATONVM_DBG_JITC=1` names why `trsbox` and `bobyqb` never speed up:

```
[cratonvm-jitc] bg-compile ...BOBYQAOptimizer.trsbox(...)[D tier=C2 optimized=true osr_bci=2713
[cratonvm-jitc] OSR-recompile reason=no-cached-artifact ...trsbox(...)[D entry_pc=2713
[cratonvm-jitc] OSR-compile FAILED ...trsbox(...)[D osr_bci=2713 — method marked OSR-denied for the rest of this process
```

Same for `bobyqb`. Both methods are called **once per test** (each JUnit
method makes one `optimizer.optimize(...)` call), so method-entry tier-up
never applies — their hot loops have no compilation door other than OSR, and
OSR refuses both permanently on first trigger.

**Why OSR refuses them:** neither method has a local exception table (`javap`
confirms no `Exception table` section for either), so this is *not* the
already-documented RBC.6b (any method with an exception table). It is the
**sibling rule, RBC.6** — `vm/src/runtime/interpreter/jit_bridge.rs`,
`compile_osr_artifact`, guards against OSR-compiling any method that contains
a bare `athrow` (even one that propagates out uncaught, no local `catch`):

```rust
// RBC.6 — never OSR an athrow method: the OSR bail path resumes
// interpretation at the back-edge, so an athrow lowering that ran
// side effects natively before throwing could see them re-applied.
```

`javap -c` on `BOBYQAOptimizer.class` confirms both `trsbox` and `bobyqb`
contain `athrow` (translated-from-Fortran numerical code that throws e.g.
`MathIllegalStateException`/`PathIsExploredException` on internal
assertion failures) with no matching local exception table — exactly the
RBC.6 shape.

## This is the same structural gap as an already-documented sibling, with a different trigger

`docs/known-issues/jit/osr-refuses-any-method-with-an-exception-table-20260817.md`
(found the same day) diagnoses the identical structural problem — "a
once-invoked method whose hot loop has no compile door but OSR, and OSR
refuses it" — for RBC.6b (exception table present). Its measurements
(19,000-309,000x slower per-iteration than HotSpot on a denied loop) and its
"What would fix it" analysis (stage the reason-9 `PendingException`
machinery for OSR, make the stale-resume fallback provably unreachable for an
exception exit, *then* lift the refusal) describe RBC.6b specifically, but
the root problem — *OSR cannot yet guarantee a precise, side-effect-safe
resume when an exception is involved* — is the same one RBC.6 exists to
guard against for the athrow-without-local-handler case. This is a concrete,
real-world (not microbenchmark) witness of that same class of cost:
`BOBYQAOptimizerTest`'s optimizer calls interpret their entire trust-region
solve, for an algorithm whose whole reason for existing is to be fast.

## Reproduction

```bash
CV="<worktree>/target/release/cratonvm.exe"
JDK="<jdk25>"
CP="<see apps/commons-math/RESULTS-20260817.md>"
RUNNER="<CratonRunner.java from apps/netty-suite-runner/, compiled standalone>"

# Times out (no result within 15-90s); HotSpot finishes in well under 1s/method:
timeout 90 "$CV" --java-home "$JDK" --Xmx 1g -c "$RUNNER;$CP" CratonRunner \
  org.apache.commons.math4.legacy.optim.nonlinear.scalar.noderiv.BOBYQAOptimizerTest

# Name the OSR refusal:
CRATONVM_DBG_JITC=1 timeout 25 "$CV" --java-home "$JDK" --Xmx 1g -c "$RUNNER;$CP" \
  CratonRunner org.apache.commons.math4.legacy.optim.nonlinear.scalar.noderiv.BOBYQAOptimizerTest \
  2>&1 | grep -iE 'trsbox|bobyqb'

# Watch it actually making (slow) forward progress, not deadlocked:
timeout 40 "$CV" --java-home "$JDK" --Xmx 1g --stack-dump-on-timeout 20 -c "$RUNNER;$CP" \
  CratonRunner org.apache.commons.math4.legacy.optim.nonlinear.scalar.noderiv.BOBYQAOptimizerTest
```

## What would fix it

Not attempted here — same staged approach as the RBC.6b sibling doc's "What
would fix it" section: extend the reason-9 `PendingException` deopt-frame
machinery to OSR compiles, make the stale-resume fallback unreachable for an
exception exit specifically (compile-time refusal instead of a runtime
reject), and only then lift RBC.6 for bare-`athrow` methods too. Whoever
picks this up should read the sibling doc's fix plan in full before touching
either gate — it explicitly warns that a naive lift trades a throughput bug
for a silent wrong-answer bug.

## Related

* `docs/known-issues/jit/osr-refuses-any-method-with-an-exception-table-20260817.md`
  — RBC.6b, the sibling gate, same underlying problem, its own fix plan
  applies here too.
* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
