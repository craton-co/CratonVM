# `String.format("%tb", ...)` panics CratonVM's native formatter: month index 12 into a 12-element array

## Status
**OPEN, not root-caused to the exact native site.** Confirmed real — this is
CratonVM's own internal panic message, not a Java-level exception; there is no
possible HotSpot equivalent to A/B against (HotSpot would just format the
string).

## The failure

```
org.apache.juli.TestOneLineFormatterPerformance.testDateFormat

java.lang.InternalError: JIT dispatch into java/lang/String.format(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String; failed: internal error: native method panic: index out of bounds: the len is 12 but the index is 12
	at org.apache.juli.TestOneLineFormatterPerformance$StringFormatImpl.format(TestOneLineFormatterPerformance.java)
```

This is CratonVM surfacing a Rust-side `panic!` (out-of-bounds slice/array
index inside the native that implements `String.format`) as a Java
`InternalError`, rather than crashing the process outright — the harness
records this class as a normal test FAIL, not a CRASH, which is presumably why
it hasn't been separately triaged as a VM-internal defect before.

## The exact trigger

```java
private static class StringFormatImpl implements DateFormat {
    public String format(long timestamp) {
        return String.format("%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS", Long.valueOf(timestamp));
    }
}
```

`%tb` is `java.util.Formatter`'s "locale-specific abbreviated month name"
conversion. **"the len is 12 but the index is 12" is an exact off-by-one**:
whatever native array CratonVM's `String.format`/date-formatting
implementation uses to look up month names has 12 valid slots (indices 0-11,
one per month), and something computed index `12` — one past the end — to
look up the month for this particular `timestamp`.

## Why this is worth a dedicated page rather than folding into a "flaky test" bucket

- It's not timing-sensitive, not contention-sensitive, not host-load-sensitive
  — the format string and its single `long` argument are fully deterministic
  inputs. Whether it panics should depend only on which month/timestamp value
  is passed in, not on the host.
- It's a native-code panic, not a Java-level logic bug — this is CratonVM's
  own `String.format` native implementation being wrong for at least one
  input shape, not a netty/tomcat bug being exercised.
- The "index == length" shape is the same signature this session's testing
  has already found once elsewhere today (the ecj `StackMapFrameCodeStream`
  bug, now FIXED — see
  `fixed-suite-bugs/ecj-stackmapframe-aioobe-was-an-int-keyed-hashmap-that-never-reported-a-change-FIXED-20260827.md`)
  — worth checking whether this is the SAME underlying mechanism (some shared
  off-by-one helper, e.g. an int-keyed collection or index-computation
  utility both native paths route through) before assuming it's unrelated.

## Not yet done, and two standalone repro attempts that did NOT reproduce it

Two standalone probes, both against the identical format string and a
`Long.valueOf(timestamp)` argument exactly matching the failing test's call
shape:

1. 400 timestamps stepped ~30 days apart (spanning ~33 years, so every month
   sampled many times over) — **no panic**.
2. 3,000,000 identical calls with the same timestamp in a tight loop (to reach
   whatever JIT tier the real test's timing loop would reach) — did not
   confirm either way (the probe was still running when it hit a 90s cutoff;
   not yet re-run uncapped).

So the trigger is not simply "any `%tb` call" and not simply "enough identical
calls to get JIT-compiled" — at least not in the exact shape tried. Differences
from the real test not yet controlled for: the real call goes through
`StringFormatImpl implements DateFormat`, i.e. an **interface-dispatched**
`format(long)` call inside a tight benchmark timing loop (`doTestDateFormat`
loops for a *fixed wall-clock duration*, not a fixed count, so the actual
iteration count and the JIT tier reached depend on host speed) — not a direct
static-context loop like both probes above. Also not controlled: the JUnit
harness's locale/timezone setup, which may differ from a bare `cratonvm.exe`
invocation's default.

- Not determined which specific `timestamp` value(s), if value-dependent,
  trigger index 12.
- Not traced to the actual Rust source implementing this — no
  `CRATONVM_DBG_*` flag tried yet for native-panic diagnostics.
- Not checked whether other `Formatter` conversions with similarly-sized
  lookup tables (`%tB` full month name, `%ta`/`%tA` day-of-week — 7 slots,
  `%tp` am/pm — 2 slots) share the same off-by-one, or whether it's specific
  to the abbreviated-month table.
- Next repro attempt should replicate the interface-dispatch shape and the
  fixed-wall-clock-duration loop exactly, since that's the one structural
  difference between what's been tried and what the failing test actually does.

## Repro

```powershell
cd apps\tomcat-suite-runner
.\run-tomcat-suite.ps1 -Vm craton -Category all -Start <index-of-TestOneLineFormatterPerformance> -Count 1 -GcFlag '-XX:+UseZGC' -RunName repro -TimeoutSec 90
```
Confirmed reproducing through the full test harness. Standalone (non-JUnit)
repro attempts have NOT succeeded yet — see above.
