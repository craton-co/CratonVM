# JIT/OSR: an exception from a compiled callee makes the OSR'd loop re-run iterations

**Status: OPEN.** Silent wrong answers, deterministic, on default settings. Not
a regression — reproduced on unmodified `dev` — and independent of the RBC.6
precise-handler-frame gate (it reproduces with that gate off, on a method whose
handler reads only parameters).

## Symptom

A loop running in OSR-compiled code calls a compiled method that throws. The
exception surfaces at the OSR return, `try_osr` takes the "safe reject" path
(`return None` = "OSR rejected, keep interpreting THIS frame from where it
was"), and the loop iterations the OSR'd code had already committed are
**executed again** — the interpreter frame's induction variable and accumulators
were never updated by the OSR'd code.

```
cratonvm/Count3.run()   20,000 iterations, callee throws on 1 in 3
  real JDK 25            iters=20000  checksum=280463264
  cratonvm (any config)  iters=20008  checksum=-206531626
```

`CRATONVM_DBG_OSR=1` shows exactly the events that account for it:

```
[cratonvm-osr] BAIL pending_exception cratonvm/Count3.run()I entry_pc=4 exc_class=…$Boom
… 5 of them, and the loop runs 8 extra iterations
```

## Reproduction

The callee lives in the committed fixture
`vm/tests/resources/cratonvm/JitPreciseHandlerFrame.java` — `plainStep` catches
its own `Boom`, and its handler reads only parameters, so it compiles with or
without `precise_handler_frames_enabled`. The driver:

```java
package cratonvm;
public class Count3 {
  static int iters = 0;
  static int run() {                       // this method is the one that OSRs
    int c = 0;
    for (int i = 0; i < 20000; i++) {
      iters++;
      c = c * 31 + JitPreciseHandlerFrame.plainStep(i % 97, (i % 3) == 0);
    }
    return c;
  }
  public static void main(String[] a) {
    int c = run();
    System.out.println("iters=" + iters + " checksum=" + c);
  }
}
```

The loop must be in a *called* method, not in `main` — with the loop directly in
`main` the counters come out right, so any repro that inlines it into `main`
will look clean.

## Why

`try_osr`'s pending-exception drain (`vm/src/runtime/interpreter.rs`, the
`[cratonvm-osr] BAIL pending_exception` site) re-stashes the exception and
returns `None`. The comment on that path is explicit that a safe reject is
"correct only when the bail precedes any committed loop iteration" — which is
true for the unconditional-at-header OSR-exit trigger it was written for, and
false here: the exception can surface at any invoke, arbitrarily far into the
loop.

Recovering the OSR'd frame's progress needs a precise frame at the throwing
invoke, which is exactly what `DeoptReason::PendingException` publishes — but
only for methods compiled with `precise_exception_frames`, i.e. methods that
declare a handler reading non-parameter locals. `Count3.run` has no exception
table at all, so nothing snapshots its loop state.

## Interaction with the precise-handler-frame gate

Turning `precise_handler_frames_enabled` on (default since 2026-07-28) widens
the population of *compiled* methods that throw and catch internally, so it
widens exposure to this defect — but does not cause it. Measured on the same
binary: `iters=20008` with the gate on and with
`CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES=1`, and the same 20008 on an unmodified
`dev` build.

## Candidate fixes

* Publish an exceptional frame at every invoke of an OSR-compiled body, not only
  in methods with a qualifying exception table, and route the OSR bail through
  the existing OSR-exit transfer instead of the safe reject.
* Or make the safe reject honest: refuse OSR entry (or mark the artifact
  not-entrant on first such bail) for a loop whose body can raise an exception
  that escapes to the OSR frame, so the progress-discarding path is never taken
  twice.

The first is the real fix; the second bounds the damage.
