# `MemoryUsage.toString()` renders a different format from HotSpot

| | |
|---|---|
| **Status** | OPEN |
| **HotSpot** | `init = 528482304(516096K) used = 9583856(9359K) committed = … max = …` |
| **CratonVM** | `init=16777216, used=3568912, committed=4294967296, max=4294967296` |
| **Discovered** | 2026-08-11, while fixing the MXBean open-type conversion |

## Symptom

```java
ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().toString()
```

HotSpot's `MemoryUsage.toString()` emits space-separated `key = value(NNNK)`
pairs, with each byte count followed by its kibibyte rounding. CratonVM emits
comma-separated `key=value` with no kibibyte suffix.

Anything that logs a `MemoryUsage` reads differently, and anything that parses
one — a diagnostic scraper, a log assertion — sees a format that exists on no
real JVM.

## Cause

`java.lang.management.MemoryUsage.toString()` is natively overridden in
`native-builtins/src/jmx.rs`, alongside `<init>`, `getInit`, `getUsed`,
`getCommitted` and `getMax`. Those exist because CratonVM synthesises
`MemoryUsage` instances directly; the four field slots the getters read (0..3 =
init/used/committed/max) already match the real class's layout, so the real
`toString()` bytecode would have the values it needs.

Likely the same shape as the MXBean type-mapping overlay retired on the same
day: a shim that outlived the gap it covered. Not verified — nobody has tried
dropping just the `toString` registration and running the real bytecode.

## Scope

Not a regression: reproduced identically on binaries from either side of the
2026-08-11 MXBean open-type fix.

Cosmetic in the sense that no API contract states the format, but
`MemoryUsage.toString()` output is stable across every real JVM and is treated
as such in practice.

## Reproduction

```bash
source /data/toolchain/env.sh
<cratonvm> --java-home /data/toolchain/jdk-25 -cp <probe-dir> MxSurvey
java -cp <probe-dir> MxSurvey          # HotSpot control
```

The `newPlatformMXBeanProxy` line at the end of `MxSurvey` prints a
`MemoryUsage` through `toString()`.
