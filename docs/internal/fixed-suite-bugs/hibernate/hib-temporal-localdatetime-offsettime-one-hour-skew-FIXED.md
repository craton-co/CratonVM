# Hibernate `type.temporal.LocalDateTimeTest` / `OffsetTimeTest` — round-tripped values consistently off by exactly 1 hour

| | |
|---|---|
| **Status** | ✅ **FIXED** on branch `fix/hib-temporal-1hour-skew-20260706` (native-builtins/src/lib.rs, `tz_standard_offset_seconds`), merged to `dev`. |
| **Area** | `java.util.TimeZone.getTimeZone(String)`'s real-JDK-mode synthetic-zone bypass (`alloc_synth_timezone` / `tz_standard_offset_seconds`, HIB-CV-34 family). |
| **Severity** | was medium — deterministic, systematic; root cause turned out to be the same known-limitation class as HIB-CV-34, just missing zone-table entries. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage. |
| **Fixed** | 2026-07-06. |

## Root cause

Exactly the HIB-CV-34 class of bug (`docs/internal/hibernate-bugs/run-20260622/HIB-CV-34-jdbc-timezone-timestamp-offset.md`),
just for two zone ids that weren't yet in the table.

`TimeZone.getTimeZone(String)` in real-JDK mode is backed by CratonVM's
`alloc_synth_timezone` (native-builtins/src/lib.rs), which allocates a real
`sun/util/calendar/ZoneInfo` object and sets its `rawOffset` field from a
small hardcoded IANA-id → standard-UTC-offset table
(`tz_standard_offset_seconds`) — because CratonVM can't load the real JDK's
`tzdb.dat` (`ZoneInfoFile.<clinit>` fails). Any zone id **not** in that table
silently gets `rawOffset = 0` (i.e. treated as UTC) instead of its real
offset.

The Hibernate `type.temporal` test suite (`Timezones.java`) parameterizes
tests over several JVM-default and `hibernate.jdbc.time_zone` zones,
including `Europe/Oslo` and `Europe/Amsterdam` — **neither was in the
table**, so `TimeZone.getTimeZone("Europe/Oslo").getRawOffset()` returned
`0` instead of the correct `3600000` (CET, +1h standard time). Since
Hibernate's `TimestampJdbcType`/`TimeJdbcType` bind with an explicit
`Calendar` when `hibernate.jdbc.time_zone` is set
(`PreparedStatement.setTimestamp(idx, ts, Calendar)`), and H2's own internal
zone resolution for the Calendar-less/default path goes through a
completely separate mechanism (`java.time.ZoneId`/`TimeZoneProvider`, not
affected by this bug), the missing Oslo/Amsterdam offset broke exactly the
`hibernate.jdbc.time_zone=Europe/Oslo` parameterized case while the
`jdbcTimeZone=null`/`GMT` cases (which don't touch the broken table) stayed
correct — producing the exact "off by 3600s, always in the same direction"
signature described in the original report.

**Confirmed NOT a JIT bug** (reproduces identically under `--nojit`) and
**confirmed NOT an H2 or Hibernate bug** — both round-trip correctly given a
`TimeZone` object with the right `rawOffset`.

## Fix

Extended the existing `tz_standard_offset_seconds` match table
(native-builtins/src/lib.rs) with the missing CET-family zone ids used by
this test suite: `Europe/Oslo`, `Europe/Amsterdam`, plus several other
common CET cities from the same standard-offset family (`Brussels`,
`Vienna`, `Copenhagen`, `Stockholm`, `Zurich`, `Warsaw`, `Prague`,
`Budapest`) to reduce the odds of the next test/app hitting the same gap.
Purely additive — new match arms on an existing `Some(3600)` value, no
existing zone's behavior changed.

**Known limitation (pre-existing, inherited from HIB-CV-34, not addressed
here):** the table is a finite, hand-maintained list of standard (non-DST)
offsets — it does not, and structurally cannot, cover the full IANA tzdb,
and it has no DST-transition data (`transitions` stays null). Any zone id
not in the table still silently defaults to UTC; any date inside a
covered zone's DST period gets the standard (winter) offset instead of the
DST offset. The real fix — loading `tzdb.dat` so `ZoneInfoFile`'s own real
bytecode path works — is a materially bigger undertaking, already flagged
in HIB-CV-34's doc as "tracked separately if needed." Not re-opening that
here; add more zone entries as they're found to matter, same as this fix
did.

## Verification

- Isolated `java.util.Calendar`/`TimeZone` repro (`TimeZone.getTimeZone("Europe/Oslo").getRawOffset()`,
  encode fields→millis, decode millis→fields) now matches real JDK 25
  HotSpot exactly (was `0`, now `3600000`).
- Isolated raw-JDBC repro (H2, `PreparedStatement.setTimestamp(idx, ts, Calendar)` /
  `ResultSet.getTimestamp(idx, Calendar)`, no Hibernate) now round-trips
  correctly for `Europe/Oslo` (was off by exactly 3600s), using the exact
  same value from the original bug report.
- Single-case clone of `LocalDateTimeTest` (all 3 `hibernate.jdbc.time_zone`
  parameterizations: null, GMT, Europe/Oslo) — was 4 ok / 1 failed (the
  Oslo case, `expected: <2017-11-06T19:19:01> but was: <2017-11-06T18:19:01>`,
  matching the original report byte-for-byte), now 8 ok / 0 failed.

## Repro (pre-fix, for reference)

```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home <jdk25> --Xmx 1500m @common.args \
  -Dcraton.batch=1 CratonRunner <(printf 'org.hibernate.orm.test.type.temporal.LocalDateTimeTest\norg.hibernate.orm.test.type.temporal.OffsetTimeTest\n') 0
```

## Cross-reference

[hib-temporal-gc-lambda-native-stale-local.md](hib-temporal-gc-lambda-native-stale-local.md)
— that doc's `type.temporal.*` cluster is a GC-corruption/CRASH family,
unrelated to this FAIL/assertion-only bug (different root cause, both now
independently tracked).

## UPDATE 2026-07-07 — DST-transition half of the known limitation FIXED (branch `fix/hib-temporal-placeholder-dup-20260707`)

The "Known limitation" above ("any date inside a covered zone's DST period gets
the standard (winter) offset instead of the DST offset") became the dominant
residual once the separate `values (??,??)` SQL-placeholder corruption was fixed
(see `hib-temporal-sql-parameter-placeholder-duplication-FIXED.md`):
`LocalDateTimeTest` failed 6/162 with the exact 1-hour skew on DST-period /
DST-boundary dates (`expected: <2018-10-28T01:00> but was: <2018-10-28T00:00>`,
plus the 2018-03-25 / 2018-04-01 / 2018-09-30 EU and New Zealand transitions).

**Fix** (`../../../../native-builtins/src/lib.rs`): `alloc_synth_timezone` now constructs a
real `java.util.SimpleTimeZone` (full 13-arg constructor, real-bytecode
`getOffset(long)`/`inDaylightTime` semantics) for any zone whose CURRENT
recurring DST rule is in the new `tz_dst_rule` table — the EU rule (last Sunday
March 01:00 UTC → last Sunday October 01:00 UTC, +1h) for the whole CET family
plus London/Athens/Bucharest/Helsinki, and the New Zealand rule (last Sunday
September 02:00 wall → first Sunday April 03:00 wall, +1h) for
`Pacific/Auckland`. Any constructor failure falls through to the legacy
transitions-less ZoneInfo path — never worse. Zones without an entry (e.g.
`America/Santiago`, whose historical rule changes SimpleTimeZone cannot model
in one recurrence) keep the standard-offset approximation; historical (pre-rule-
change) dates in covered zones are still approximated by the CURRENT rule.

**Verification:** `scratch-min/TzDstProbe.java` (winter/summer offsets, exact
before/after instants at the 2018 EU and NZ transitions, Calendar round-trip,
untouched-zone regression guards) — expectations validated on HotSpot JDK 25,
then `@@PASS` on the fixed CratonVM binary. `LocalDateTimeTest` DST-skew
failures 6 → 0 (see the placeholder-duplication doc for the full class table).
