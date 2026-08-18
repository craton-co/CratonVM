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
already refused there. The residual case would be an `athrow` OUTSIDE every
protected range of a method that has one elsewhere, and it stays refused because
`athrow`'s lowering still publishes no frame to route by; that is a lowering
gap, not a policy one.

### The wrong-answer bug that residual uncovered, also fixed here

Reasoning about that case found a live defect in the freshly-landed RBC.6b lift,
independent of `athrow`. `route_osr_exception_out_of_artifact` asks this method's
own table with the PRECISE throw bci and can answer `Propagate` — "this frame
cannot catch". `OsrBackoffOutcome::ThrowJava` then handed the throwable to
`unwind_to_handler` keyed on `entry_pc`, the BACK-EDGE the compiled body was
ENTERED at, which has nothing to do with where the throw happened. With the loop
inside the `try` (`try { for (..) {..} } catch`) that back-edge IS inside a
protected range, so the unwinder found the `catch`, entered it, and resumed the
frame on the stale pre-OSR locals.

`probes/OsrThrowOutsideTryProbe.java` — a loop inside a `try`, a `trip()` throw
after it — measured against HotSpot on the same host:

```text
HotSpot   caught=0 escaped=1 sink=80000200000
CratonVM  caught=1 escaped=1 sink=80018203000     <- before
CratonVM  caught=0 escaped=1 sink=80000200000     <- after
```

Both halves were silent: an exception swallowed by a handler that does not guard
it, and an accumulator **18 003 000 too high** from the iterations the spurious
resume re-ran. Nothing raised, and no termination test could see either.

The fix is `exception_dispatch::OSR_FRAME_DECLINED_TO_CATCH` — a pc no
`[start_pc, end_pc)` can contain, handed to `unwind_to_handler` in place of
`entry_pc` at all fifteen `ThrowJava` sites. The first search matches nothing,
the frame pops, and `exc_pc` is re-read from the caller's `last_instr_pc` as
usual. Saying it in the existing signature rather than adding a parameter is
deliberate: the router has ALREADY asked this frame with better information, so
what the unwinder needs is not another opinion but a pc that cannot produce one.

`probes/OsrExcTableProbe.java` (the RBC.6b lift's own acceptance probe) stays
line-for-line identical to HotSpot after the fix, with
`osr_exception_handler_entered=210` beside `osr_entered=213` — the lift's real
handler entries are untouched; only the decline path changed.

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
* **An `athrow` in a table-bearing method is still refused** — not for the
  unwind reason (fixed above) but because `athrow`'s lowering publishes no
  precise exceptional frame, so a throw from inside a protected range has
  nothing to route by. Admitting it needs the lowering to publish, which is the
  same work `first_unsupported_precise_frame_site` lists `athrow` under.
* `local_oop_masks` carries the same 64-slot truncation this page fixed for
  liveness — **audited 2026-08-18, and it is SAFE, but not for the reason its
  comment gave.** Worth recording in full, because the comment named a gate that
  does not exist and a reader could have deleted a real one looking for it.

  The truncation is worse than "a slot beyond bit 63":
  `compute_local_oop_masks` returns **empty vectors** for `max_locals > 64`, so
  `is_oop` reads false for EVERY local in such a method, slot 0 included.
  Measured on `probes/HighLocalOopProbe.java` (84 locals, references at slots
  74/75/76, OSR-entered, 100 forced collections): `oop_reached=false
  oop_mask=0x0`.

  `can_deopt_resume` is `!deopt_points.is_empty() && !has_elided_monitor` and
  says nothing about the local count, so it is not what saves this. Three other
  things do:

  1. **`classify_local_kinds` has no 64-slot cap**, and `deopt_real_enabled()`
     defaults ON, so `local_kinds` is populated in production and its
     `LocalKind::Ref` arm publishes `RegisterRef`/`StackSlotRef` at any slot
     index. In the same measured frame, locals 74/75/76 came out
     `StackSlotRef(-600/-608/-616)` — correct — with the mask entirely dark. A
     slot the classifier calls `Ambiguous` publishes `Unsupported`, which is
     fail-closed. **This is the leg with no other guard behind it**, so it is
     now pinned by
     `x64::deopt_snapshot_tests::classify_local_kinds_types_a_reference_above_slot_63`,
     which asserts both halves together and goes red if the classifier is ever
     narrowed to a `u64` to match its neighbours.
  2. **The two gates move together.** Under `CRATONVM_DEOPT_REAL=0`,
     `local_kinds` is empty — and so is the snapshot: no deopt point is recorded
     for the method at all (measured: zero `[excframe] FRAME` lines), and
     `osr_exit_points` / `can_osr_exit` are empty/false, so nothing consumes one.
  3. **The GC side refuses explicitly and observably.**
     `moving_young_safepoint_coverage_complete` carries its own
     `num_locals > 64 => false`, and its doc states that a false result is "a
     correctness signal to the GC: if this frame is live here, moving-young must
     divert to the non-moving sweep for that cycle". Under
     `CRATONVM_MOVING_YOUNG=1` with `CRATONVM_DBG=moving-young-coverage-dbg` the
     probe prints `[moving-young-coverage] incomplete: active frame map at
     rbp=…` **49 times**; `probes/LowLocalOopProbe.java`, the identical shape
     under 64 locals, prints it **0 times** and gets a real mask
     (`oop_reached=true oop_mask=0x700000e`). The local count is the only
     difference between the two, which is what makes that the `num_locals > 64`
     rule firing rather than a coincidence. Separately, `color_graph` caps at 64,
     so a local above slot 63 is always frame-resident and the conservative sweep
     — which pins rather than relocates — sees it.

  Both probes pass on CratonVM and HotSpot alike at `-Xmx 512m` and `-Xmx 96m`,
  under default ZGC compaction and under `CRATONVM_MOVING_YOUNG=1`. No fix was
  needed; the misleading comment at the `is_oop` site has been corrected in
  place.

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
iteration counts in both. `vm/tests/jit_osr_throw_outside_try.rs` is the
regression test for the unwind fix, with the same anti-vacuity guard (a run that
never OSR-compiled `afterLoop` proves nothing, because the interpreter gets this
right for free).

## Related

* fixed-suite-bugs/jit/osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md
  — RBC.6b, the sibling gate, lifted the same day. Its `loopEscapes` probe arm
  notes that it had to throw through a callee because RBC.6 refused an inline
  throw; that constraint is gone.
* known-issues/perf/bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md
  — what actually costs `BOBYQAOptimizerTest` its 90 seconds.
* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
