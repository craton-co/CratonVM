# `DateFormatSymbols.getProviderInstance` fails codegen

**Status:** 🔴 **OPEN**, found 2026-07-31 while re-deriving
[tomcat/32.4](../../internal/fixed-suite-bugs/tomcat/32-doc04-residual-perf-assertions-CLOSED.md).

The VM classifies this one itself. `CRATONVM_DBG_JIT_METHOD_STATS=1` on
`probes/DateFmtProbe`:

```
[cratonvm] JIT method stats: 1 hot method(s) whose COMPILE FAILED
           (not policy — these are bugs):
[cratonvm]  12000 queued=false tier_fail_count=3
   java/text/DateFormatSymbols.getProviderInstance(Ljava/util/Locale;)Ljava/text/DateFormatSymbols;
```

`tier_fail_count=3` with `ineligible=false` is the tiered manager's way of
saying the backend was asked three times and failed three times — as opposed to
a skip-list decline, which is recorded once and never retried. Under
`CRATONVM_DBG_JITC=1` it surfaces as a bare backend failure with no named
refusal site:

```
[cratonvm-jitc] bg-compile java/text/DateFormatSymbols.getProviderInstance(...)
                tier=C1 optimized=false
[cratonvm-jitc] compile-bail java/text/DateFormatSymbols.getProviderInstance(...)
                backend_attempted=true
```

`backend_attempted=true` distinguishes it from the `resolver-bail site=…`
family (doc 30) — the resolver was fine and codegen itself gave up. Nothing
names *why*, which is the first thing to fix: doc 30's experience is that a
bail reporting only `backend_attempted` reads like a transient resolver miss
and gets ignored.

## Reproduction

```
set CRATONVM_DBG_JIT_METHOD_STATS=1
cratonvm.exe -Xmx2g -Dprobe.iters=2000 -cp <probes-out> DateFmtProbe
```

Both `DateFmtProbe` and `DateSymbolsProbe` reach it (the latter more directly:
its A and B loops call `DateFormatSymbols.getInstance` and
`new DateFormatSymbols(Locale)` respectively).

## Scale, and a warning about what fixing it buys

It is genuinely hot — 12 000 invocations in a 2 000-iteration probe run, ~2 per
`String.format` call, because `String.format`'s `%t` conversions resolve locale
date symbols. Interpreted, it costs real time: `DateFormatSymbols.getInstance`
measures 50–62 µs against HotSpot's 1.1–1.2 µs.

**But do not fix this expecting `TestOneLineFormatterPerformance` to pass.** It
sits on `String.format`, which is that test's *fast* side — the side the
assertion races against. Speeding it up makes that test **harder**. Fix it
because a hot JDK method failing codegen is a defect worth understanding, and
because `String.format` is broadly hot across the suites.

## Update 2026-07-31 — not the cause on the SLOW side either, measured

The obvious next thought is that the *other* compile-bails on that path make
`SimpleDateFormat.format` slow. `CRATONVM_DBG=jit-method-stats` on
`DateFormatPatternProbe` lists six:

```
100015  tier_fail_count=3  java/text/DateFormatSymbols.getProviderInstance(...)
 40000  tier_fail_count=3  sun/util/locale/provider/CalendarDataUtility.retrieveFieldValueName(...)
 39495  tier_fail_count=3  java/text/DecimalFormat.format(JLjava/text/Format$StringBuf;...)
  1972  tier_fail_count=3  java/text/NumberFormat.getInstance(...)
  1498  tier_fail_count=3  sun/util/locale/provider/JRELocaleProviderAdapter.getNumberFormatProvider()
   999  tier_fail_count=3  java/text/DecimalFormatSymbols.clone()
```

**They are not the cause.** `probes/SdfOnlyProbe.java` runs
`SimpleDateFormat.format` and nothing else, with pattern `"ss"` — a single
2-digit numeric field, which reaches none of them — and costs **51 µs with
`hot_but_stuck_in_interpreter=0`**: zero compile failures anywhere on the path,
and still 870× HotSpot's 59 ns. (The counts above are also partly the probe's
own direct calls, not `SimpleDateFormat`'s — `DecimalFormat.format`'s 39 495 is
close to that probe's own 40 000 explicit invocations.)

The cost is the generic-dispatch round trips the format performs. See
[30 § Adopted](../../internal/fixed-suite-bugs/tomcat/30-hot-loop-jit-admission-bans-testmethodperformance-CLOSED.md#adopted-2026-07-31--two-residuals-from-the-retired-tomcat32-and-where-they-went)
and [raw JIT-to-JIT](../jit-raw-jit-to-jit-shadow-stack-overflow-20260731.md),
which now carries that work.

So: fix this because a hot JDK method failing codegen is a real defect. Stop
citing it as a lever for any date-formatting throughput test — on either side.
