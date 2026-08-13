# `MemoryUsage.toString()` rendered a format that exists on no real JVM

| | |
|---|---|
| **Status** | FIXED 2026-08-11 |
| **HotSpot** | `init = 528482304(516096K) used = 0(0K) committed = … max = -1(-1K)` |
| **CratonVM** | now identical, including the kibibyte column and the `-1(-1K)` undefined case |
| **Discovered** | 2026-08-11, while fixing the MXBean open-type conversion |
| **Fixed in** | `fix/memoryusage-tostring-format-20260811` |

## Symptom

```
HotSpot  : init = 528482304(516096K) used = 0(0K) committed = 528482304(516096K) max = 8413773824(8216576K)
CratonVM : init=528482304, used=0, committed=528482304, max=8413773824
```

Comma-separated, no spaces around `=`, and no kibibyte column. Anything that
logs a `MemoryUsage` — which is most diagnostics that touch
`MemoryMXBean`/`MemoryPoolMXBean` — read differently here, and anything that
scraped one saw a shape no real JVM produces.

## Root cause

`java.lang.management.MemoryUsage.toString()` was natively overridden in
`native-builtins/src/jmx.rs`, next to the `<init>`, `getInit`, `getUsed`,
`getCommitted` and `getMax` natives that exist because CratonVM synthesises
`MemoryUsage` instances directly.

Those getters have a reason to be native; `toString` did not. `MemoryUsage` is a
self-contained JDK class, and its four fields are declared

```java
private final long init;
private final long used;
private final long committed;
private final long max;
```

— in exactly the order this file writes them by index, so the real bytecode
reads the same slots the getters hand back. The shim was reimplementing
something already available and getting the format wrong: the real method emits
`key = value(NNNK) ` pairs with `value >> 10` as the kibibyte column.

Same shape as the MXBean type-mapping overlay retired the same day: a shim that
outlived the gap it covered.

## Fix

`memoryusage_tostring_shim_enabled()` in `native-builtins/src/jmx.rs`. Default:
the class's own bytecode. Gated, not deleted — `synthetic-jdk` builds keep the
shim (no bytecode to fall back to) and
`CRATONVM_SYNTHETIC_MEMORYUSAGE_TOSTRING=1` restores it on a real-JDK run.

The shim's own format is corrected to match, so a synthetic-jdk run is not a
second, different divergence rather than the same one.

## Verification

`MuProbe` on both VMs. Every deterministic line diffs clean:

```
explicit= init = -1(-1K) used = 0(0K) committed = 1024(1K) max = -1(-1K)
zeros   = init = 0(0K) used = 0(0K) committed = 0(0K) max = 0(0K)
getters = 1,2,3,4
rendered= init = 1(0K) used = 2(0K) committed = 3(0K) max = 4(0K)
```

The two cases worth having in the probe are the ones a naive format string gets
wrong: a negative (undefined) value, where `-1 >> 10` is `-1` and not `0`, and a
value below 1024, which must render `(0K)` rather than being omitted.

The getters still agree with the rendered text (`1,2,3,4` above), which is the
check that the real bytecode is reading the slots the natives write, and
`getAttribute` still returns `CompositeDataSupport` — the 2026-08-11 open-type
fix is unaffected.

`cratonvm-native-builtins` suites green.

## Residual noted, out of scope

CratonVM's memory pools report `-1` for every field
(`pool[Eden Space] = init = -1(-1K) used = -1(-1K) …`), i.e. UNDEFINED rather
than a measurement. That is the pool implementation having no numbers to
report, not a rendering problem, and it is unchanged by this fix.
