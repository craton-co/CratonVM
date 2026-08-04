# `OffsetDateTimeTest`/`ZonedDateTimeTest` — a compiled "checkcast erased-T; athrow" can fail to throw, swallowing the real exception behind a `NullPointerException`

**Status:** OPEN (2026-08-04). New CratonVM bug, unrelated to timezone/date
correctness and unrelated to the moving-young mechanism these two classes
were previously (and separately) flagged for. Root-caused to a narrow,
tier-transition-timing-sensitive defect in `athrow`'s new optimizing-tier
(`Op::Throw`) lowering, landed by COV-07
(`docs/internal/cov-07-athrow-RETIRED-20260804.md`, commit `12b8cbdea`,
2026-08-04) — this is the first day that lowering could ever be reached, and
the first day these two classes show this symptom. Confirmed with a minimal,
Hibernate-free, HotSpot-diffed repro below; not fully bisected to one
instruction, but strongly localized (JIT-only, and the trigger pattern is
exactly the one COV-07 newly admits).

## Symptom, today's fresh run

`apps/hib-suite-runner/runs/run-20260804-113511-custom/on-real/` (dev tip
`a43a74ded`, `CratonVM-hib-local-0712-v3`), real-JDK, JIT on, default flags:

```
org.hibernate.orm.test.type.temporal.OffsetDateTimeTest FAIL found=488 ok=320 failed=15 aborted=153  (shard-0/raw.log)
org.hibernate.orm.test.type.temporal.ZonedDateTimeTest  FAIL found=608 ok=404 failed=28 aborted=176  (shard-2/raw.log)
```

None of the 15/28 failures are a real Hibernate/temporal assertion. Every
one is a `NullPointerException` thrown from **JUnit Platform's own internal
exception-dispatch machinery**, in one of two shapes:

```
@@TESTFAIL org.hibernate.orm.test.type.temporal.OffsetDateTimeTest writeThenNativeRead(SessionFactoryScope) FAILED
java.lang.NullPointerException: Cannot throw exception because the return value of
  "org.junit.platform.commons.util.ExceptionUtils.throwAsUncheckedException(java.lang.Throwable)" is null
	at org.junit.jupiter.engine.descriptor.JupiterTestDescriptor.invokeExecutionExceptionHandlers(JupiterTestDescriptor.java:119)
	...
	at org.junit.jupiter.engine.descriptor.TestMethodTestDescriptor.invokeTestExecutionExceptionHandlers(TestMethodTestDescriptor.java:232)
```

```
@@TESTFAIL org.hibernate.orm.test.type.temporal.OffsetDateTimeTest nativeWriteThenRead(SessionFactoryScope) FAILED
java.lang.NullPointerException
	at org.junit.jupiter.engine.descriptor.TestMethodTestDescriptor.cleanUp(TestMethodTestDescriptor.java:175)
```

Both call chains bottom out in JUnit Platform's `ExceptionUtils`, which
implements the standard "sneaky throw" idiom to rethrow a checked exception
from a context that can't declare it:

```java
public static RuntimeException throwAsUncheckedException(Throwable t) {
    ExceptionUtils.<RuntimeException>throwAs(t);
    return null;                         // unreachable IF throwAs really throws
}
private static <T extends Throwable> void throwAs(Throwable t) throws T {
    throw (T) t;                         // checkcast(erased) ; athrow
}
```

`throwAs`'s `throw (T) t` compiles to `checkcast <erased-to-Throwable>` then
`athrow` — a **`void`**-returning method whose entire body is a checked-cast
immediately followed by an unconditional throw. When this fails to actually
throw, `throwAsUncheckedException` falls through to `return null;`, and
whichever caller does `throw ExceptionUtils.throwAsUncheckedException(t);`
gets Java's own auto-generated helpful-NPE message for throwing a `null`
reference — exactly the first message above. **The real, original exception
that JUnit was trying to rethrow is silently lost**; only its replacement
NPE is visible in the log, which is why no genuine per-test failure detail
survives for any of these 43 (15+28) failures.

Both classes show only this NPE shape; searching all 4 shards' raw logs for
`throwAsUncheckedException` found it **only** in these two classes'
`@@TESTFAIL` blocks (24 occurrences in `OffsetDateTimeTest`'s shard, 54 in
`ZonedDateTimeTest`'s) — nowhere else in today's 50-class run.

## Why only these two classes, today

`OffsetDateTimeTest`/`ZonedDateTimeTest` are unusually long single-process
JUnit runs (488 and 608 discovered tests respectively, 14-15 wall-clock
minutes each). JUnit Platform's own dispatch/cleanup/handler-chain code
(which routes through `ExceptionUtils.throwAsUncheckedException` on
essentially every abnormal test completion) runs an enormous number of times
within one class. The minimal repro below shows the defect firing roughly
**once per ~600-700 compiled invocations** of the affected method shape —
rare enough that a short-lived class would need to get unlucky, but with
hundreds of tests each exercising this machinery multiple times (per-test
setup, teardown, extension callbacks), a long class reliably rolls the dice
enough times to hit it repeatedly. This is a plausible, evidence-consistent
explanation for why only the two longest-running classes in today's residual
list show it — it does not require anything temporal-specific, and the
minimal repro below confirms the defect has nothing to do with dates,
timezones, or Hibernate.

## Minimal, Hibernate-free, HotSpot-diffed repro

```java
public class SneakyThrowProbe {
    static RuntimeException throwAsUnchecked(Throwable t) {
        SneakyThrowProbe.<RuntimeException>throwAs(t);
        return null; // unreachable if throwAs really throws
    }
    @SuppressWarnings("unchecked")
    private static <T extends Throwable> void throwAs(Throwable t) throws T {
        throw (T) t;
    }
    public static void main(String[] args) {
        int npes = 0;
        for (int i = 0; i < 300000; i++) {
            try {
                throw throwAsUnchecked(new RuntimeException("probe" + i));
            } catch (NullPointerException npe) {
                npes++;
                System.out.println("NPE (sneaky throw failed to throw) at i=" + i + ": " + npe);
            } catch (RuntimeException e) { /* expected */ }
        }
        System.out.println("done npes=" + npes);
    }
}
```

Real HotSpot JDK 25 control (`n=300000`): **`npes=0`**, every run.

CratonVM `--nojit` (`CratonVM-hib-local-0712-v3` @ `a43a74ded`, `n=300000`):
**`npes=0`**.

CratonVM default (JIT on), 4 runs:

| run | npes | first (only) NPE at iteration |
|---|---:|---:|
| 1 | 1 | i=615 |
| 2 | 1 | i=653 |
| 3 | 1 | i=609 |
| 4 | 1 | i=655 |

100% reproduction rate across 4 runs, always exactly one occurrence per run,
always in a tight ~600-655 iteration window — consistent with a
tier-transition/first-compiled-invocation race rather than a steady-state
miscompile (a steady-state bug would recur many times per run once the
method is hot; this fires once, right around where the JIT would first
compile+install the method, then never again for the remaining ~299,400
iterations). A 5th run with `CRATONVM_DBG=ir-compiles` (which confirmed both
`throwAsUnchecked` and `throwAs` get `admitted to the optimizing pipeline`
and get a compiled body) showed **0** npes — consistent with the extra
stdout I/O perturbing the timing enough to dodge the race window, not with
the bug being absent.

## Why COV-07 is the leading suspect

`git log a9241eedf..a43a74ded` (the range between the 2026-08-03 clean-pass
verification recorded in
`../../internal/fixed-suite-bugs/hibernate/moving-young-inert-under-jit-throughput-tax-20260730-RETIRED.md`
— `OffsetDateTimeTest failed=0`, `ZonedDateTimeTest failed=0` — and today's
build) includes `12b8cbdea feat(jit): COV-07 — athrow gets a real IR
lowering, closing the last ir_compatible conjunct`. Per
`docs/internal/cov-07-athrow-RETIRED-20260804.md`: before this commit, any
method containing an `athrow` was unconditionally refused optimizing-tier
(IR) admission (`scan.has_athrow` conjunct) — `throwAs`'s exact shape
(`checkcast <erased>; athrow`, nothing else) could **never** have reached
the new `Op::Throw` lowering path before today. After the commit, it can.
The regression window (clean 08-03 -> broken 08-04) lines up exactly with
when this code path first became reachable at all.

Source inspection of the new lowering (`jit/src/ir_lower.rs`,
`Op::Throw` arm, `lower_terminator`) did not find an obvious logic bug: it
loads the exception ref, calls `jit_throw_exception(exc_ptr, bci)` (which
`vm/src/jit/helpers.rs` confirms always returns `i64::MIN`), and
unconditionally jumps to the shared `call_exc_patches` bail stub that every
other exceptional `Op::Call`/`Op::CheckCast` exit already uses
(`emit_call_return_check`'s sentinel-compare correctly covers `IrType::Void`
via its generic `CMP ; JE` branch, not the `Long/Double/Float`-only ambiguous
path). The mechanism *should* be sound in steady state — which is consistent
with the failure being narrowly timing-window-shaped (once per run, near
first compilation) rather than a straightforward always-wrong lowering. The
COV-07 doc's own "What this lane does NOT own" section separately flags a
**known, pre-existing, unrelated** gap in the same shared stub (missing
`jit_set_throw_bci` stamping for `Op::Call`/`Op::CheckCast` exceptional
exits, affecting `finally` handler resolution) — not investigated further
here since it doesn't explain a `void`-returning helper failing to throw at
all, only a handler picking the wrong bci once something has already thrown.
**Root cause not fully nailed down**: the leading hypothesis is a race in
compiled-code *installation*/first-invocation (e.g. a newly-installed entry
point invoked while some patch/relocation for the `call_exc_patches` jump
target is still in flight), not a defect in the lowering's steady-state
logic — but this was not independently confirmed via disassembly or a
targeted installation-race test in this session.

## Not the moving-young mechanism

Both classes still show the previously-documented `[moving-young] fallback`
WARN lines (8-9 and 8 respectively, `reason=innermost-rbp-belongs-to-
unguarded-callee`/`unregistered-jit-frame-on-stack`/etc., checked strictly
within each class's own log region) — moving young collections are still
requested but always fall back to the non-moving sweep, exactly as recorded
in `moving-young-inert-under-jit-throughput-tax-20260730-RETIRED.md`'s final
verification. This is **not** the cause of today's `FAIL` status: neither
class hangs or hits any internal timeout — both reach `@@RESULT` and every
failure is one of the NPEs above, not a `TimeoutException`. Wall time is
`OffsetDateTimeTest` 859s (~14.3 min) and `ZonedDateTimeTest` 476s (~7.9
min); `ZonedDateTimeTest` is inside the 08-03 doc's recorded 391-447s range,
but `OffsetDateTimeTest` is well above its recorded 308-544s range —
plausibly ordinary shared-host contention (this doc's own final verification
explicitly flags this host as prone to 6+-way concurrent `cratonvm*`
contention) rather than a new mechanism, but not independently confirmed
quiet-host here; worth a re-time if the gap matters. Either way, GC fallback
behavior is orthogonal to the sneaky-throw NPEs. See that doc's own status
(RETIRED, correctly so — confirmed this session, no live conflict with the
`docs/known-issues/` copy, which no longer exists on disk; it was properly
moved there on 2026-08-03).

## Repro (Hibernate path, for reference)

```
cd apps/hib-suite-runner
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 cratonvm.exe --java-home "<jdk25>" \
  --Xmx 1500m @common.args -Dcraton.batch=1 CratonRunner \
  org.hibernate.orm.test.type.temporal.OffsetDateTimeTest
```

Prefer the standalone `SneakyThrowProbe.java` above for iteration — it's a
~1-second-per-100k-iterations loop, no Hibernate/H2/JUnit bootstrap cost, and
already confirmed to reproduce the defect at the same order of magnitude.

## Next steps (not done this session)

- Bisect `a9241eedf..a43a74ded` directly against `SneakyThrowProbe.java`
  (binary search on the commit range) to confirm COV-07's commit
  specifically, rather than the correlational argument above.
- Instrument the `call_exc_patches` bail-stub jump target and the method's
  installation/patching sequence to look for a window where the `E9 rel32`
  emitted by `Op::Throw`'s lowering could be read/executed before its
  relocation is fully patched, or where the newly-JIT-compiled entry could
  be invoked before the compile is fully "published" to other threads/the
  dispatch table.
- Re-run `SneakyThrowProbe.java` with `CRATONVM_JIT_TIER_THRESHOLD`-style
  levers (if any exist) to see whether forcing compilation earlier/later
  shifts the ~600-655 iteration window proportionally (would confirm the
  tier-transition-timing hypothesis) or leaves it fixed (would argue against
  it).
