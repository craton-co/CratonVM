# `ZoneId.systemDefault()` host-timezone-leak — FIXED 2026-07-17

**Status: FIXED** (partial closure of `ZonedDateTimeTest`). Root-caused to
a stale real-JDK-mode native bypass for `java.time.ZoneId.systemDefault()`
that unconditionally returned a hardcoded synthetic UTC `ZoneOffset`,
ignoring every `java.util.TimeZone.setDefault(...)` call the running Java
program had made. This reduced `ZonedDateTimeTest`'s failure count from
63/608 to 20/608 (verified stable across 4 solo reruns); the remaining 20
are a distinct, narrower, pre-1911 `Europe/Paris` Local-Mean-Time
precision gap — see the "Residual" section below and
`hib-misc-residuals-20260716-FIXED.md`.

## Background

This bug was hidden behind an unrelated GC livelock
(`hib-aqs-threadpoolexecutor-relocation-livelock-FIXED.md`,
fixed earlier the same day, commit `62be72a0`) that made
`ZonedDateTimeTest`/`LocalDateTimeTest` un-runnable in isolation on any
host. Once that livelock stopped masking execution, `ZonedDateTimeTest`
could finally complete — and revealed 63/608 parameterized-test failures,
all `AssertionFailedError`s showing a clean N-hour timestamp skew, e.g.:

```
expected: <2017-11-06 09:19:01.0> but was: <2017-11-06 01:19:01.0>
```

## Root cause

`org.hibernate.orm.test.type.temporal.Timezones.withDefaultTimeZone(env, runnable)`
(the shared test helper used by every `@Test` method in
`AbstractJavaTimeTypeTests` and its `ZonedDateTimeTest`/`LocalDateTimeTest`
subclasses) does:

```java
TimeZone.setDefault(toTimeZone(env.defaultJvmTimeZone()));
// ... run the test body in a new Thread via ExecutorService ...
```

The test body then computes its own "expected" values by calling
`ZoneId.systemDefault()` directly (e.g.
`getOriginalZonedDateTime().withZoneSameInstant(ZoneId.systemDefault())`),
and Hibernate's own `ZonedDateTime`/JDBC-timestamp binding internally goes
through `TimeZone.getDefault()`. Both are supposed to reflect the same,
settable, JVM-level default zone — on real HotSpot, `ZoneId.systemDefault()`
literally delegates to `TimeZone.getDefault().toZoneId()`.

In CratonVM's real-JDK mode, `ZoneId.systemDefault()` is intercepted by a
native bypass registered in `register_essential_natives`
(`../../../../native-builtins/src/lib.rs`, originally landed as "Round 13" to work
around a *different*, still-open bug: the real JDK bytecode path for
`ZoneId.systemDefault()` walks through `TimeZone.getDefault()` →
`ZoneInfo.getTimeZone(...)` → `sun/util/calendar/ZoneInfoFile.<clinit>`,
which fails to load `${java.home}/lib/tzdb.dat` and gets silently
swallowed, leaving `ZoneId.systemDefault()` returning `null` and NPEing
log4j's `ParameterFormatter.<clinit>` during boot). That original fix
was correct in intent (never let this call return `null`) but always
returned the *same* hardcoded UTC `ZoneOffset` object, no matter what
`TimeZone.setDefault(...)` had been called with:

```rust
// before:
let s = ctx.create_string("Z");
ctx.set_field(obj, 0, Value::Int(0));       // totalSeconds = 0
ctx.set_field(obj, 1, Value::Object(Some(s))); // id = "Z"
Ok(Some(Value::Object(Some(obj))))
```

So every `ZoneId.systemDefault()` call in the whole VM process — including
inside `Timezones.withDefaultTimeZone()`'s freshly-spawned executor
thread, well after `TimeZone.setDefault(GMT-08:00)` had run — kept
resolving to UTC. Since `TimeZone.getDefault()` is *also* natively
overridden nearby ("Round 54") but correctly tracks `setDefault(...)`,
this produced a hard split: Hibernate's actual JDBC write/read path
(`TimeZone.getDefault()`-based, correct) computed one instant, while the
test's own expected-value computation (`ZoneId.systemDefault()`-based,
stuck on UTC) computed a different one — off by exactly the configured
zone's UTC offset. `writeThenRead` (round-trips entirely through
Hibernate, using the *same* correct `TimeZone.getDefault()` path on both
write and read) mostly canceled the error out and passed; `writeThenNativeRead`/
`nativeWriteThenRead` (which each swap one side for a raw
JDBC/`ZonedDateTime.of()`/expected-value computation going through the
broken `ZoneId.systemDefault()`) surfaced the mismatch on nearly every
non-UTC default-zone parameter.

Confirmed directly with a minimal probe
(`TimeZone.setDefault(GMT-08:00)` on the main thread, then read both APIs
from inside an `Executors.newSingleThreadExecutor()` task, mirroring
`Timezones.withDefaultTimeZone()` exactly):

```
=== before (unfixed) ===
before: TimeZone.getDefault()=UTC ZoneId.systemDefault()=Z
after setDefault (main thread): TimeZone.getDefault()=GMT-08:00 ZoneId.systemDefault()=Z
inside executor thread: TimeZone.getDefault()=GMT-08:00 ZoneId.systemDefault()=Z
```

## A dead-code detour

The obvious-looking fix location is `../../../../native-builtins/src/util_time.rs`,
which has its own, much more complete, `ZoneId`/`TimeZone`/`Clock`
synthetic implementation — including a *second*, already-tzdata-aware
`native_zone_id_system_default_tzdata` registration explicitly commented
"Overrides the earlier hardcoded-UTC registration." That function was
edited first, and compiled cleanly, but had **zero effect** on the actual
`cratonvm-cli` binary: `util_time.rs::register_time_natives` is only ever
called from `register_synthetic_overrides`, which is itself only called
from `register_builtins`, which is itself only reachable behind
`#[cfg(feature = "synthetic-jdk")]` in `../../../../vm/src/vm/vm_init.rs` — a cargo
feature the default real-JDK `cratonvm-cli` build does not enable. The
*actual* winning registration in the shipped binary is the one in
`native-builtins/src/lib.rs::register_essential_natives`, called
unconditionally in both build modes. (`util_time.rs`'s version was fixed
too, for consistency/any future synthetic-jdk build, but it is not what
ships today.)

## Fix

`../../../../native-builtins/src/lib.rs`, the `ZoneId.systemDefault()` registration
inside `register_essential_natives`:

1. First call `TimeZone.getDefault()` (`ctx.invoke`, hitting the
   already-correct "Round 54" native override) and `.getID()` on the
   result.
2. Hand that id string to the *real* bytecode `ZoneId.of(String)` —
   unregistered/native-free in real-JDK mode, so it runs the JDK's actual
   zone-rules parsing/construction, already proven correct elsewhere in
   this same test suite for explicit `ZoneId.of(...)` calls (DST rules
   included, unlike a hand-rolled fixed-offset approximation).
3. Only if any step in that chain fails, fall back to the original
   hardcoded UTC `ZoneOffset` construction verbatim — preserving the
   log4j-boot NPE guard this bypass exists for.

`../../../../native-builtins/src/util_time.rs`: applied the equivalent fix (factored
into a shared `jvm_default_zone_id()` helper) to
`native_zone_id_system_default_tzdata` and `Clock.systemDefaultZone`, for
parity in any build that does enable `synthetic-jdk`.

## Verification

Fixed binary, real-JDK mode, real `/home/victor/jdk25`, solo runs:

- Probe (`TimeZone.setDefault` + cross-thread `ZoneId.systemDefault()`
  read): now matches real HotSpot exactly — `GMT-08:00` on both APIs,
  both before and after crossing into the executor thread.
- `ZonedDateTimeTest`: `found=608 started=608 ok=384 failed=20 aborted=204
  skipped=0` — stable across 4 independent solo reruns (2 pre-merge, 1
  post-merge onto the then-current `dev` tip, 1 with `-Dcraton.trace=1`).
  Down from `failed=63` on the pre-fix binary. All 20 residual failures
  are a distinct, narrower bug (pre-1911 `Europe/Paris` Local-Mean-Time
  `+00:09:21` offset precision, occasionally manifesting as
  `java.lang.CloneNotSupportedException` instead of a plain
  `AssertionFailedError` depending on GC/timing) — see the residuals doc
  for the full breakdown; NOT fixed by this change and tracked
  separately.
- `LocalDateTimeTest`: `found=162 started=162 ok=90 failed=0 aborted=72
  skipped=0` — unaffected/no regression (this class's default-zone
  parameters never happened to hit the same code path in a
  failure-visible way, or its assertions are less sensitive to the
  40-vs-63 split; either way it was already `failed=0` before this fix
  and stays that way after).
- `InstantTests`: `found=204 started=204 ok=112 failed=0 aborted=92
  skipped=0` — no regression.

## Related

- `hib-aqs-threadpoolexecutor-relocation-livelock-FIXED.md`
  — the GC-relocation livelock that was masking this bug entirely until
  fixed earlier the same day.
- `hib-misc-residuals-20260716-FIXED.md` —
  `ZonedDateTimeTest` entry updated with this fix and the new, narrower,
  still-OPEN pre-1911 Paris LMT-offset residual.
