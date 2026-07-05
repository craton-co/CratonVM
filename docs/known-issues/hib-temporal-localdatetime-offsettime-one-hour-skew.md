# Hibernate `type.temporal.LocalDateTimeTest` / `OffsetTimeTest` — round-tripped values consistently off by exactly 1 hour

| | |
|---|---|
| **Status** | 🔴 OPEN — not root-caused. TimeZone-default hypothesis explicitly ruled out (see below). Comparison to HotSpot is inconclusive (HotSpot itself `ABORTED` on these classes), so CratonVM-specificity is **not fully confirmed**, but the failure pattern is too clean/systematic to ignore. |
| **Area** | Unknown — somewhere in the `LocalDateTime`/`OffsetTime` ⇄ JDBC `Timestamp`/`Time` round-trip path (write via `PreparedStatement`, read back via `ResultSet`). |
| **Symptom** | `org.opentest4j.AssertionFailedError: Writing then reading a value should return the original value ==> expected: <X> but was: <X minus exactly 1 hour>` — for every single failing case, in both classes. |
| **Severity** | medium — deterministic, systematic, affects two full temporal-type test classes if reproducible outside this harness's HotSpot-ABORTED caveat. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |

## Symptom

Every failing assertion in both `LocalDateTimeTest` and `OffsetTimeTest` has
the exact same shape: the value read back is **exactly 60 minutes earlier**
than the value written, with zero exceptions across dozens of distinct
dates/times:

```
LocalDateTimeTest:
  expected: <2017-11-06T19:19:01> but was: <2017-11-06T18:19:01>
  expected: <1970-01-01T00:00>    but was: <1969-12-31T23:00>
  expected: <1900-01-01T00:00>    but was: <1899-12-31T23:00>
  expected: <2018-10-28T01:00>    but was: <2018-10-28T00:00>   (etc.)

OffsetTimeTest:
  expected: <09:19:01Z> but was: <08:19:01Z>
  expected: <23:59:59Z> but was: <22:59:59Z>
  expected: <00:00Z>    but was: <23:00Z>     (wraps across midnight)
  expected: <01:00Z>    but was: <00:00Z>
```

Critically, the 1900 and 1970 dates are **long before any DST rule could
apply** in any real timezone's history — this is not a DST-transition bug,
it's a flat, constant −3600-second shift applied unconditionally.

Two distinct secondary failures also appear in each class's failure list
(not the 1-hour skew, likely downstream/unrelated noise from repeated
SessionFactory rebuilds across parameterized cases):
```
java.lang.RuntimeException: Could not build SessionFactory: Cannot invoke "java.lang.ThreadGroup.getMaxPriority()" because "g" is null
org.hibernate.exception.GenericJDBCException: Error calling Driver.connect() [... NullPointerException: Cannot invoke "java.util.Iterator.hasNext()" because "<local3>" is null ...]
```
These look like harness/rebuild flakiness under a shared H2 in-memory DB
across many parameterized re-executions within the timeout window, not part
of the core 1-hour bug — flagged here for completeness, not yet
investigated further.

## What's ruled out

**Host timezone is not the cause.** The Azure host is UTC:
```
$ timedatectl
Time zone: Etc/UTC (UTC, +0000)
```
**CratonVM's own `TimeZone.getSystemTimeZoneID` native stub also reports UTC
unconditionally** (`native-builtins/src/lib.rs`, `native_timezone_get_system_id`
returns the literal string `"UTC"` — this is the real-JDK-mode default-zone
native CratonVM must supply since real JDK bytecode calls it during
`TimeZone.getDefault()`/`System.initPhase1` bootstrap). So neither the OS
nor CratonVM's own zone-detection code has any UTC+1 bias that would
directly explain a systematic 1-hour skew.

The `native-builtins/src/util_time.rs` synthetic `ZoneId`/`ZoneRules`
implementation (which *does* have a hardcoded `Europe/Paris|Berlin|Rome|Madrid|CET → 3600s`
table) is very likely **not involved** — this suite runs in real-JDK mode
(`--java-home`, real classpath), where `java.time`/`java.sql` classes execute
as real JDK bytecode, not CratonVM's synthetic-mode fallback objects. This
should be double-checked directly (e.g. a debug log/breakpoint confirming
whether any synthetic `ZoneId` path is actually hit here) rather than
assumed, since it's the one piece of code in the tree with exactly a
3600-second constant tied to timezones.

**HotSpot comparison is inconclusive, not clean.** The Azure HotSpot
baseline run `ABORTED` on both classes rather than cleanly passing —
`LocalDateTimeTest`: `found=162 ok=90 failed=0 aborted=72`; `OffsetTimeTest`:
`found=396 ok=176 failed=0 aborted=88 skipped=132`. Zero HotSpot failures
where it did run assertions is suggestive (real Hibernate/H2 correctly
round-trips these values), but the high abort count means this HotSpot run
itself hit some instability (likely fixture/teardown related, possibly the
same shared-DB flakiness noted above) — this is not a clean apples-to-apples
"HotSpot 100% passes, CratonVM 100% fails" comparison the way the other docs
in this sweep have. Treat the CratonVM-specificity of the core 1-hour skew
as **likely but not proven** until a clean, non-aborted HotSpot run is
captured for these two classes specifically.

## Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(printf 'org.hibernate.orm.test.type.temporal.LocalDateTimeTest\norg.hibernate.orm.test.type.temporal.OffsetTimeTest\n') 0
```

## Next steps (not yet done)

- Get a clean (non-aborted) HotSpot baseline for just these 2 classes in
  isolation (single-shard, generous timeout) to confirm 0 failures with no
  interference from the abort noise seen in the full 194-class sweep.
- Narrow to a single minimal repro (one `LocalDateTime` value, insert +
  select) and binary-search which layer introduces the −1h shift: JDBC
  `PreparedStatement.setObject`/`ResultSet.getObject` for
  `TIMESTAMP`/`TIME` columns, `java.sql.Timestamp`/`Time` construction from
  `LocalDateTime`/`OffsetTime` (`Timestamp.valueOf`, `Time.valueOf`, or
  Hibernate's `JdbcTimestampJavaType`/`OffsetTimeJavaType` bind/extract
  code), or a JIT-miscompiled arithmetic pattern in one of those paths
  (check with `--nojit`).
- If `--nojit` also reproduces it, this is very likely NOT a JIT bug and
  points at a native stub or a real-JDK-mode `Calendar`/`Timestamp`
  bootstrap path that's silently substituting a synthetic/fallback value —
  worth grepping for any other unconditional `3600` constant near
  date/time/JDBC native code besides the one already ruled out in
  `util_time.rs`.
- Cross-reference [hib-temporal-gc-lambda-native-stale-local.md](hib-temporal-gc-lambda-native-stale-local.md)
  — that doc's `type.temporal.*` cluster is a GC-corruption/CRASH family,
  distinct from this FAIL/assertion-only symptom, but worth a quick check
  in case both trace back to the same temporal-value plumbing.
