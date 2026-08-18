# `BOBYQAOptimizerTest`'s hot loop is refused OSR by RBC.6 (bare `athrow`, no local handler) — FIXED, and this page's own conclusion was wrong

**Status: the named gate is FIXED 2026-08-17** on
`perf/osr-athrow-lift-20260817`, together with two further gaps the lift
uncovered. **The workload it was written about is NOT fixed**, and the reason is
not the one this page gave — see "What this did NOT fix" below and the open page
it hands off to.

`RBC.6` refused every OSR compile of a method containing a bare `athrow` (0xbf).
OSR is the only door out of the interpreter for a method invoked once — a
`@Test` body, a `main`, any one-shot driver — so a `throw` anywhere in such a
method, even on a path never taken, kept its hot loop interpreted for the
method's whole life.

## What the fix is

**Lift RBC.6 when the method declares no local exception handlers.** The
refusal's own justification was that "the OSR bail path resumes interpretation
at the back-edge, so an athrow lowering that ran side effects natively before
throwing could see them re-applied" — RBC.7's silent-corruption shape, and true
when it was written. What makes the lift safe is a precondition checkable at the
door rather than new machinery: with an EMPTY exception table an `athrow` cannot
be caught by the OSR'd frame, so no drain has to resume that frame at all.
`route_osr_exception_out_of_artifact` answers `Propagate` on its first line for
exactly this population, the throwable goes to the dispatch loop's unwinder as
`OsrBackoffOutcome::ThrowJava`, the frame is torn down, and there is no stale
resume for committed iterations to be re-run from.

`CRATONVM_JIT_OSR_ATHROW=0` restores the blanket refusal, so ONE binary A/Bs the
lift.

**A non-empty table stays refused, and not for RBC.6b's reason.** Since RBC.6b's
own 2026-08-17 lift, a method with a table is admitted whenever every throwing
site inside a protected range publishes a reason-9 frame — and `athrow`'s
lowering is one of the few that does not, so an `athrow` INSIDE a `try` is
already refused there. The residual case is an `athrow` OUTSIDE every protected
range of a method that has one elsewhere. There
`route_osr_exception_out_of_artifact` correctly answers `Propagate` (no precise
frame, so the throw site is outside every range), but `ThrowJava` then hands the
throwable to `unwind_to_handler` keyed on `entry_pc` — the BACK-EDGE the body
was entered at, not the throw site. When that back-edge lies inside a protected
range (`try { for (..) {..} } catch`), the unwinder finds a handler that does not
cover the throw and enters it on the stale pre-OSR locals. Until `ThrowJava`
carries "this frame has already declined to catch", admitting that shape trades a
throughput bug for a wrong-answer bug. **That gap is live for the RBC.6b lift
too**, independently of `athrow`: any implicit exception raised outside every
range in a table-bearing OSR'd method takes the same path.

### Two more gaps, both found only because the lift opened the door

Lifting RBC.6 got `trsbox`/`bobyqb` COMPILED and changed nothing, because OSR
ENTRY was then refused at every back edge. One undescribable slot in one deopt
point makes that point's frame unresumable, and `osr_exit_policy` refuses the
whole artifact for it — so each of these cost an entire method its OSR:

1. **`local_liveness` was one `u64` per pc**, so a method with more than 64
   locals could not drop a DEAD local above slot 63. A slot the whole-method
   kind classifier had to call `Ambiguous` — javac reusing one slot for an `int`
   in one region and a `double` in another, which is ordinary — then published
   `FrameValue::Unsupported`. `regalloc::live_locals_per_pc_all` runs the same
   analysis once per 64-slot WINDOW; window 0 is bit-for-bit the old answer, so a
   method with 64 locals or fewer is unchanged.

   The census is what named it: **every** undescribable slot in `trsbox`
   (86/87/89) and `bobyqb` (64/67/68) is above 63 and **none** is below it. That
   is the shape of a truncated mask, not of a real analysis failure.

2. **The `StackSlot::Xmm` arm of the operand-stack snapshot never consulted the
   typed operand stack.** Its sibling arms (frame slot, GPR) both grew a
   `stack_kinds` fallback when that width source landed; this one kept its
   original "float-vs-double is not recoverable from the abstract stack alone"
   comment, which had stopped being true. So an FP operand that happened to be
   REGISTER-resident — the ordinary case in FP code, which is where the XMMs are
   — published `Unsupported` while its spilled twin published a precise
   `StackSlotDouble`.

   Witness: `trsbox` bci 628, a `getEntry(I)D` call with a live `dload`ed double
   under the receiver, which `stack_kinds` calls `[Double, Ref, Int]` and which
   agrees with the emitter's own oop marks (`CRATONVM_DBG_STACK_KINDS=1` prints
   `accepted=true` there).

## The evidence

`probes/OsrAthrowProbe.java`, `n = 400 000`, ONE binary, the gate as the only
variable. `control` is `coldThrow` with the `throw` statement deleted and is what
makes the column a measurement rather than a wall-clock guess:

| arm | `coldThrow` ns/iter | `control` ns/iter |
|---|---:|---:|
| HotSpot 25 | 7 | 8 |
| CratonVM, lift ON | **13** | 10 |
| CratonVM, `CRATONVM_JIT_OSR_ATHROW=0` | **388** | 11 |

**~29x on the shape the gate governs, with the control unchanged.** (Measured
again on a quieter host: 8 vs 213 ns/iter, control 6 both ways — the ratio is
stable, the absolute numbers are not, because this host builds other worktrees.)

The correctness half is the primary measurement, not a footnote: the refusal
existed to prevent a re-run, and a re-run is a wrong answer no termination test
sees. Every arm, and HotSpot, report `takenThrow.effects=240001` and
`nestedThrow.effects=240001` **exactly** — the count of iterations that really
ran, incremented before the throw can fire, so a stale resume at the back-edge
reads HIGH here and nowhere else.

The compile verdicts, which is what the OPEN page was reading:

```
# before
[cratonvm-jitc] osr-DENY (RBC.6 athrow, handlers=0) OsrAthrowProbe.coldThrow(I)J
[cratonvm-jitc] OSR-compile FAILED OsrAthrowProbe.coldThrow(I)J osr_bci=4 — method marked OSR-denied …
# after
[cratonvm-jitc] OSR-compile OsrAthrowProbe.coldThrow(I)J entry_pc=4 len=1396
[cratonvm-jitc] OSR-compile OsrAthrowProbe.takenThrow(II)V entry_pc=2 len=1865
```

`probes/OsrDenyShapeProbe.java`'s six shapes all compile and its `sink` matches
HotSpot; `probes/OsrExcTableProbe.java` (the RBC.6b lift's own acceptance probe)
is line-for-line identical to HotSpot on this branch.

And the three gates in sequence on `BOBYQAOptimizer`, one `optimize()` call.
Entry refusals are exact counts, not timings, so a loaded host cannot move them:

| after | `trsbox` OSR entry refusals | dominant reason |
|---|---:|---|
| (before the lift) | — | never compiled: `OSR-compile FAILED` |
| RBC.6 lift | 43 060 | `osr-entry-undescribable-slot` — local 86/87/89 |
| + high-local liveness | 21 109 | `osr-entry-unresumable-exit` — `stack 0 (Unsupported) of 3` |
| + XMM stack kinds | 21 109 | `osr-entry-unresumable-exit` — a still-`Ambiguous` LIVE local |

with `osr_entered=1465 osr_exited=0` on the last row: the entries that do happen
run to completion and never bail.

## What this did NOT fix, measured — and it is the whole point of the page

`BOBYQAOptimizerTest` still does not pass, and **the OSR refusal was never why**.
One `optimize()` call on the 12-dim Rosenbrock (the core of `testRosen`), same
host, same run:

| | ms per `optimize()` |
|---|---:|
| HotSpot 25 | **494 – 621** |
| CratonVM, JIT on | 42 988 – 46 117 |
| CratonVM, `--nojit` | 50 792 – 53 610 |

**The JIT buys ~15% on this workload and HotSpot is ~80x faster than either.**
`CRATONVM_DBG=jit-method-stats` says `hot_but_stuck_in_interpreter=0` — a real
answer, not a missing instrument: everything hot IS compiled. `--stack-sample-ms
25` over 1 297 samples puts ~57% of the time in `ArrayRealVector.getEntry`,
`Array2DRowRealMatrix.getEntry` and `setEntry` — one-line array accessors HotSpot
folds into a single load — counting both their bodies and the invoke cost
attributed to their entry frames. Even deleting all of it leaves ~12 s against
HotSpot's 0.5.

So this page's headline — "its hot loop is refused OSR … turning a sub-second
optimize call into an unbounded hang" — was a correct observation (the refusal
was real, and named accurately) attached to a wrong conclusion. The refusal was
found first because `CRATONVM_DBG_JITC=1` prints it and nothing prints the
accessor dispatch cost. **A named refusal on the path is not evidence that the
refusal is the cost.** The arm that would have caught it is `--nojit`, which this
page never ran.

The real cause has its own open page:
known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md.

## Residuals

* **21 109 entry refusals remain on `trsbox`** — a genuinely LIVE local whose
  kind is `Ambiguous` even under the per-bci reaching-kind refinement, at a bci
  where liveness cannot drop it. Closing it needs a real per-slot type oracle,
  not a wider mask. It is now the only OSR refusal left in the method, and by the
  `--nojit` row above it costs approximately nothing.
* **`OsrBackoffOutcome::ThrowJava` unwinds keyed on `entry_pc`**, not on the
  throw site — see the second section. It blocks admitting an `athrow` in a
  table-bearing method, and it is a live wrong-answer path for the RBC.6b lift
  independently of `athrow`.
* `local_oop_masks` carries the same 64-slot truncation this page fixed for
  liveness. Its own comment says a slot beyond bit 63 "reads as non-oop here —
  sound only because `can_deopt_resume` (later) gates such methods off". Not
  touched here; worth confirming that gate still holds now that a >64-local
  method can be OSR-entered.

## Repro

```bash
javac -nowarn -d . probes/OsrAthrowProbe.java
java -cp . OsrAthrowProbe 400000                                     # the control
CRATONVM_JIT_OSR_ATHROW=1 cratonvm --java-home <jdk> -cp . OsrAthrowProbe 400000
CRATONVM_JIT_OSR_ATHROW=0 cratonvm --java-home <jdk> -cp . OsrAthrowProbe 400000
# the compile verdicts, and the named refusal
CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk> -cp . OsrAthrowProbe 400000 2>&1 \
  | grep -E 'OSR-compile|osr-DENY'
# the engagement counters — read these, not the correctness lines
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> -cp . OsrAthrowProbe 400000 2>&1 \
  | grep 'OSR lifecycle'
```

`vm/tests/jit_osr_athrow_lift.rs` is the same probe as a differential test: it
runs both gate states of one binary, requires an `OSR-compile` line for
`coldThrow` in the ON arm and its absence in the OFF arm, and asserts the exact
iteration counts in both.

## Related

* fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md
  — RBC.6b, the sibling gate, lifted the same day. Its `loopEscapes` probe arm
  notes that it had to throw through a callee because RBC.6 refused an inline
  throw; that constraint is gone.
* known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md
  — what actually costs `BOBYQAOptimizerTest` its 90 seconds.
* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
