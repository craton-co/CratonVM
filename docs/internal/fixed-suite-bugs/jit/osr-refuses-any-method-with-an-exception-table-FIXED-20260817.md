# A once-invoked method whose hot loop contains a `try`/`catch` runs entirely interpreted — the OSR door refuses any method with an exception table

**Status: FIXED 2026-08-17** on `perf/osr-exception-table-and-nesting-inline-20260817`,
Azure Linux host, release build, real-JDK mode, against HotSpot 25 on the same
host. Diagnosed 2026-08-17 on `perf/netty-exhaustive-loop-walls-20260817`; the
diagnosis was right and the fix is the one that page designed.

`RBC.6b` refused **any** OSR compile of a method with a non-empty exception
table. Because OSR is the only door out of the interpreter for a method invoked
once — a `@Test` body, a `main`, any one-shot driver — "a hot loop with a
`try`/`catch` in it", ordinary Java, ran interpreted for its whole life.

## What the fix is

Exactly the three steps the OPEN page prescribed, plus the admission predicate
it named:

1. **Stage the three requests for the OSR compile.** `jit::try_compile` has
   always staged them for a method-entry compile; `compile_osr_artifact` never
   did, which is the whole of why "an OSR artifact NEVER carries handler
   ranges". Staged now at the same point — after every early return, so a
   refused attempt cannot leak a request into the next method on the worker
   thread:

   * `set_precise_exception_frame_request(true)` — every invoke inside a
     protected range publishes a **reason-9** (`DeoptReason::PendingException`)
     frame keyed on the THROWING bci. That bci is the point: the live
     interpreter frame is parked at the stale pre-OSR back-edge pc, so without
     it the handler `[start_pc, end_pc)` test runs against a pc that has nothing
     to do with where the throw happened.
   * `set_protected_ranges_request` — suppresses the sibling tail-call for a
     call inside a `try`, which would tear the frame down and `JMP`.
   * `set_pending_exception_ranges` — handler entry edges for
     `find_bypassable_loop_headers`.

2. **Make the stale-resume fallback unreachable at COMPILE time**, not at the
   exit. `compile_osr_artifact` admits a method with a table only when every
   throwing site inside a protected range publishes such a frame — which is
   `first_unsupported_precise_frame_site`, the predicate RBC.6 already uses on
   the method-entry path, **asked rather than copied** so the two doors cannot
   drift. An `ldc`, an array access, an `athrow`, a `new` or an
   `invokedynamic` inside a `try` is still refused, and now named
   (`osr-DENY (osr-exc-site-unpublished pc=… opcode=…)`) instead of failing one
   of the door's ~30 silent `return None`s.

3. **Route the exception precisely at the exit.** All four OSR bail drains
   (pending exception, NPE, AIOOBE, arithmetic) now go through one
   `route_osr_exception_out_of_artifact`, and
   `transfer_osr_exception_exit_into_live_frame` is the exception sibling of the
   in-place resume transfer: same fail-closed contract, but it parks the frame
   at a HANDLER (a reason-9 point's semantics are `RETHROW`, so
   `resume_after_exit` correctly refuses it) and does not restore the operand
   stack, which handler entry empties by definition.

   The three implicit-exception drains used to search this frame's table at
   `entry_pc` — "our best-known throw site — the back-edge OSR entry, which
   dominates the failing helper call". That was inert while RBC.6b held and is a
   silent wrong answer once it does not. `propagate_osr_exception`, which used
   "does this frame have a table?" as a proxy for "can this frame catch?" and
   answered *fall back to the stale resume* when it did, is deleted.

`CRATONVM_JIT_OSR_EXC_TABLE=0` restores the blanket refusal, so one binary A/Bs
the lift.

## The two things that made it a vacuous green, and how each was caught

Both were caught by **printing the engagement counter next to the number**, and
neither was visible in any correctness result. Every arm of
`probes/OsrExcTableProbe.java` was green — matching HotSpot, `--nojit`, and the
kill-switch arm exactly — through both of them.

**(a) An OSR exit that enters a handler was charged to the rejection budget.**
`maybe_try_osr_at_backedge` records a per-pc rejection whenever `try_osr`
answers `None`, and `should_try_osr` refuses OSR permanently at that pc after
`OSR_MAX_ATTEMPTS = 5`. The handler-entry path answers `None` — it means "keep
interpreting THIS frame, now parked at the catch block" — so the *fifth* caught
exception turned OSR off for the rest of the frame's life. On a loop with
netty's measured 7.7% throw rate that is roughly sixty-five compiled iterations
out of 4 294 967 296.

Fixed with a `committed_out` channel mirroring the existing `osr_throw` one, and
for the reason that one already states: "the OSR'd body RAN (and committed loop
iterations), so this is not a rejected attempt".

**(b) A `RETHROW` point was counted as a competing resume image, and the entry
was refused for every method the lift had just admitted.** Measured:

```
osr_entered=0  osr_refused_entry=15  osr_entry_refused_ambiguous_image=15
[cratonvm-jitc] OSR-refuse OsrExcTableProbe.loopWithCatch(I)V entry_pc=2
  osr-entry-ambiguous-exit-image (bci 11 names two resume images whose
  ResumeSemantics disagree: +0x187 (ReceiverTypeChanged, reexecute)
  vs +0x25c (PendingException, rethrow))
```

The artifact compiled and **nothing ever entered it**. A `try { foo(x); } catch
(...)` loop puts a speculative-dispatch guard and a `PendingException` frame on
the same invoke bci, which is the ordinary shape of the population the lift
admits, so this refused essentially all of it.

`osr_exit.rs`'s own module note had named this exact reachable disagreement —
`REEXECUTE` vs `RETHROW` — and drawn the wrong conclusion from it. A `RETHROW`
point is not a resume image, by the same sentence that admits it: such points
are "stashed separately (`take_exceptional_frame`) and never routed to a
resume". `resume_image` now skips them; two points that both claim to be resume
images and disagree are still refused. The note and the two admission tests that
encoded the old rule were corrected in place.

`osr_exception_handler_entered` was added to `OSR_EVENTS` so the
`[cratonvm] OSR lifecycle:` line carries the counter. Without the row
`record_osr_event` ignores an unknown name silently — a counter that cannot
fail, which is what a probe for a vacuous green must never be.

## The evidence

`probes/OsrExcTableProbe.java`, `n = 300000`, four arms. Every line identical,
which is the correctness half:

| arm | loopWithCatch | loopTwoRanges | loopEscapes |
|---|---|---|---|
| HotSpot 25 | catch=74 localSum=11063296 side=300000 body=300000 | catch=19074 localSum=13864960 | catch=65 escaped=1 |
| CratonVM, lift ON | *identical* | *identical* | *identical* |
| CratonVM, `CRATONVM_JIT_OSR_EXC_TABLE=0` | *identical* | *identical* | *identical* |
| CratonVM, `--nojit` | *identical* | *identical* | *identical* |

and the engagement half, same binary, same probe:

| | `osr_entered` | `osr_exception_handler_entered` | `osr_method_denied` |
|---|---:|---:|---:|
| lift ON | **212** | **209** | 0 |
| lift OFF | 0 | 0 | 15 |

The compile verdicts, which is what the OPEN page's shape bisect was reading:

```
# before
OSR-compile FAILED  OsrExcTableProbe.loopWithCatch(I)V  osr_bci=2 — OSR-denied for the rest of this process
OSR-compile FAILED  OsrExcTableProbe.loopTwoRanges(I)V  osr_bci=2 — OSR-denied for the rest of this process
# after
OSR-compile         OsrExcTableProbe.loopWithCatch(I)V  entry_pc=2 len=1480
OSR-compile         OsrExcTableProbe.loopTwoRanges(I)V  entry_pc=2 len=1693
```

`probes/OsrDenyShapeProbe.java`'s `tryCatchLoop` / `tryCatchDoWhile` — the two
arms that gave the original bisect its answer — compile now, with `sink`
unchanged.

Three arms of the probe are deliberate and each guards a different thing:
`handlerLocalSum` sums the loop's induction variable **inside the catch**, so a
handler entered on the live frame's stale pre-OSR locals reads as a wrong number
rather than as nothing; `bodyRuns`/`sideEffects` count one per iteration, so a
stale resume that re-runs committed iterations (RBC.7) reads as a count larger
than `n`; and `loopEscapes` throws from OUTSIDE every protected range, which is
the router's "no precise frame ⇒ propagate" deduction.

One probe-shape note for whoever edits it: the escape arm's first draft threw
inline, which RBC.6 (`has_athrow`, a separate and still-standing gate) refuses,
so that arm measured the interpreter in both binaries. It throws through a
callee now.

## What this does NOT fix, measured

A caught exception still costs a full OSR **exit and re-entry**, because the
compiled body cannot enter its own handler. `probes/OsrExcRateProbe.java` —
five identical once-invoked loop bodies differing only in throw rate, so the
rate=0 arm is the control and `(t(rate) - t(0)) × rate` is the per-throw cost:

| throw rate | HotSpot ns/iter | CratonVM ns/iter | CratonVM ns per throw |
|---|---:|---:|---:|
| 0 (control) | 1.88 | 71.91 | — |
| 1/64 | 3.77 | 83.08 | **715** |
| 1/8 | 3.91 | 400.28 | **2 627** |
| 1/1 | 8.60 | 2 994.77 | **2 923** |

against HotSpot's 6.7–16 ns per throw. The mechanism is not inferred, it is
counted: on that run `osr_entered=2280923` beside
`osr_exception_handler_entered=2280919` — one OSR round trip per caught
exception.

`perf record` on the throw-every-iteration arm shows the cost is **flat and
diffuse**, not one target: interpreted field resolution for the one interpreted
iteration each catch costs (`resolve_field_ref_loader_aware` 5.0%,
`load_class_concurrent_for` 3.4%, `is_class_initialized_via_manager` 2.5%, the
class-manager lock 2.4%, `hash_one::<&str>` + sip `write` 3.7%, `memcmp` 2.4%),
the OSR entry machinery (`try_osr_with_backoff` 3.7%, `osr_exit_policy` 3.3% —
recomputed per entry though it is a pure function of the artifact,
`route_osr_exception_out_of_artifact` 2.6%, `validate_osr_entry` 1.2%), and
8.6% in `mi_malloc`/`mi_free`. Nothing here is a 10x lever; closing it is the
compiled-exception-handler feature (enter the handler without leaving compiled
code), which HotSpot has and this VM does not.

## Repro

```bash
javac -nowarn -d . probes/OsrExcTableProbe.java probes/OsrDenyShapeProbe.java probes/OsrExcRateProbe.java
java -cp . OsrExcTableProbe 300000                                   # the control
cratonvm --java-home <jdk> -cp . OsrExcTableProbe 300000
CRATONVM_JIT_OSR_EXC_TABLE=0 cratonvm --java-home <jdk> -cp . OsrExcTableProbe 300000
cratonvm --java-home <jdk> --nojit -cp . OsrExcTableProbe 300000

# the engagement counters — read these, not the correctness lines
CRATONVM_DBG=jit-method-stats cratonvm --java-home <jdk> -cp . OsrExcTableProbe 300000 2>&1 | grep 'OSR lifecycle'
# the compile verdicts, and the named refusal for a site that cannot publish
CRATONVM_DBG_JITC=1 cratonvm --java-home <jdk> -cp . OsrExcTableProbe 300000 2>&1 | grep -E 'OSR-compile|osr-DENY|OSR-refuse'
# what a caught exception still costs
cratonvm --java-home <jdk> -cp . OsrExcRateProbe 2000000
```

## Related

* `netty/httpheadervalidationutiltest-exhaustive-loop-timeout-20260816.md` — the
  workload this was found from. Its blocker (1) is this page and is closed; its
  blocker (2), 21 ns/iteration, is not, and the per-throw table above replaces
  that page's ~600 ns estimate.
* `osr-refused-for-a-loop-inline-in-main-FIXED-20260818.md` — the same "OSR is the
  only door for a once-invoked method" structure, refused for a different
  reason.
