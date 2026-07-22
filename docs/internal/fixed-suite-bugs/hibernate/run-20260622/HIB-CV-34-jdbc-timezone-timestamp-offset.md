# HIB-CV-34 — JDBC time/timestamp time-zone offset handling is wrong

**Run:** full Hibernate ORM suite, 2026-06-22/23
**Binary:** `cvhibtest.exe` (dev `c863b23e`)
**Severity:** High — silent data corruption (wrong times); **deterministic, `--nojit`**, HotSpot PASS
**Status:** ✅ **FIXED** on branch `fix/hibernate-tz-offset` (commit `b45441fe`, off dev `f8cdd52b`)

---

## ✅ FIX (2026-06-23)

**Root cause:** real-JDK-mode native `TimeZone.getTimeZone()`/`getDefault()` bypass
(`alloc_synth_timezone` in `native-builtins/src/lib.rs`) hard-coded `rawOffset=0`
for every zone id — because `ZoneInfoFile.<clinit>` can't load `tzdb.dat`. So
`getRawOffset()`/`getOffset(long)` returned 0 for named zones (e.g.
`America/Los_Angeles`), silently shifting every custom/session time-zone JDBC
timestamp by the zone offset. (Not the JDBC `setTimestamp(Calendar)` path — that
was correct; the underlying `TimeZone` data was zero.)

**Fix:** wire the standard (non-DST) UTC offset from a self-contained IANA table
(`tz_standard_offset_seconds`, mirrors `util_time::iana_zone_offset_seconds` which
is `synthetic-jdk`-gated and absent in the default build) into the synthetic
zone's `rawOffset` field (ms). `transitions` stays null, so `getOffset(long)`
returns this offset for all instants — a standard-offset-year-round approximation
(no DST), sufficient for the failing tests.

**Verified:** `MinTZ` America/Los_Angeles `rawOffset` 0 → −28800000;
all 4 tests PASS under `--nojit` **and** JIT:
`JdbcTimestampCustomTimeZoneTest`, `JdbcTimeCustomTimeZoneTest`,
`JdbcTimestampCustomSessionLevelTimeZoneTest`, `JDBCTimeZoneZonedTest`.

**Known limitation:** no DST transitions — instants in a zone's DST period get the
standard offset (off by 1h). Full fix would load `tzdb.dat`. Tracked separately if
needed.

---

## Symptom

Multiple `timestamp` / `timezones` tests fail with **wrong time-zone offsets**
(HotSpot PASS):

| Class | expected | got |
|---|---|---|
| `...timestamp.JdbcTimeCustomTimeZoneTest` | `16:00:00` | `00:00:00` |
| `...timestamp.JdbcTimestampCustomTimeZoneTest` | `0` | `28800000` (= 8 h) |
| `...timestamp.JdbcTimestampCustomSessionLevelTimeZoneTest` | `0` | `28800000` (= 8 h) |
| `...timezones.JDBCTimeZoneZonedTest` | `...T12:32:25Z` | `...T07:32:25Z` (−5 h) |

The errors are not random: they are **fixed offsets** (8 h / 5 h / a full
zero-out), i.e. a time-zone conversion is being applied/omitted incorrectly when
binding/reading `java.sql.Time`/`Timestamp` with a custom or session-level
time zone.

## Why it's a real CratonVM bug

- Deterministic, reproduces standalone under `--nojit` (not the JIT family).
- HotSpot PASS on all four.
- Consistent fixed-offset deltas across independent tests → one underlying
  time-zone handling defect, not flakiness.

## Root cause area (hypothesis)

The tests exercise Hibernate's `hibernate.jdbc.time_zone` / per-session time-zone
handling, which uses `PreparedStatement.setTimestamp(i, ts, Calendar)` /
`getTimestamp(i, Calendar)` and `java.util.TimeZone` / `Calendar` math. A
consistent 8 h / 5 h shift points at CratonVM's `Calendar`/`TimeZone` offset
computation or the `set/getTimestamp(..., Calendar)` JDBC path applying the wrong
(or no) zone adjustment. `JdbcTimeCustomTimeZoneTest` getting `00:00:00` for
`16:00:00` suggests the time-of-day is being zeroed/normalized away entirely.

## Reproduce

```
cvhibtest.exe --java-home <jdk25> --nojit @common.args -Dcraton.trace=1 \
  CratonRunner <list-with-JdbcTimestampCustomTimeZoneTest> 0
# AssertionFailedError: expected: <0> but was: <28800000>
```

## Suggested next step for a fixer

Compare CratonVM `java.util.Calendar`/`TimeZone.getOffset(...)` and the JDBC
`setTimestamp/getTimestamp(idx, value, Calendar)` results against HotSpot for a
non-UTC zone. The 8 h figure (28800000 ms) is Asia/Shanghai/PST-class; verify
zone-offset lookup and DST handling.

## Triage

Real, deterministic, silent **wrong-data** divergence — higher concern than a
crash because it corrupts results quietly. Independent of the JIT. Hand off to
whoever owns `java.time`/`Calendar`/`TimeZone` + JDBC temporal binding.
