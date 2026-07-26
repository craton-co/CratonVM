# H2 — `java.time`/`TimeZone` zone-rule offset miscalculation (date-time correctness)

## Status
**FIXED** (2026-07-22, branch `fix/h2-timezone-zonerules-20260721`, merged to
dev). Also resolves the older duplicate/stale
`docs/internal/h2-suite-bugs/bug-h2-timezone-dst-offset.md` (never deleted,
contrary to what this doc's own OPEN version claimed — its three symptoms
(`TestDateStorage` DST-gap 8h shift, `TestTimeStampWithTimeZone` +3h,
`TestValue` +1h) are the exact same root cause, all now fixed).

## Original severity
**MEDIUM-HIGH (correctness)** — any timestamp-with-timezone conversion could
come out wrong by a non-constant, zone/date-dependent number of hours.

## Root cause

`java.time.zone.ZoneRules.getOffset(Instant)` was **already correct** — in
CratonVM's real-JDK mode, that call reaches genuine JDK bytecode (no
competing native registration for it), and real `ZoneRulesProvider`
resolution works fine. This was confirmed directly: `ZoneId.of("America/New_York").getRules().getOffset(instant)`
already returned the correct DST-aware offset before any change here.

The actual bug was in the **legacy `java.util.TimeZone`/`GregorianCalendar`**
path, which is what H2's `LegacyDateTimeUtils`/`Calendar`-based conversion
code (and `TestValue`'s `Calendar.getInstance(TimeZone.getTimeZone(...))`)
actually exercises. `TimeZone.getTimeZone(id)`'s native construction path
(`alloc_synth_timezone`) built a `sun.util.calendar.ZoneInfo`/
`java.util.SimpleTimeZone` object populated from:

- `tz_standard_offset_seconds` — a ~40-entry hardcoded table of *standard*
  (winter) offsets only, no DST.
- `tz_dst_rule` — a further ~20-entry hardcoded table modeling each zone's
  *current/modern* recurring DST rule only (e.g. "second Sunday of March"),
  with no historical transition data at all.

Any zone outside the DST-rule table (the overwhelming majority of the ~600
IANA zones) got the flat standard offset year-round — explaining the
non-constant, zone/date-dependent deltas this doc originally reported (a
6-hour delta one run, 3-hour and 1-hour deltas the time before).

Two further zone-**id-resolution** bugs in the same area surfaced while
reproducing against the live H2 suite (`org.h2.test.unit.TestTimeStampWithTimeZone.testConversions`
iterates every `TimeZone.getAvailableIDs()` entry):

- `TimeZone.getTimeZone("CST6CDT")` (and `"EST5EDT"`/`"MST7MDT"`/`"PST8PDT"`
  — the four POSIX-rule zone names `TimeZone.getAvailableIDs()` itself
  returns) silently collapsed to a bogus `"GMT"`/0-offset zone: the
  "recognized zone id" check only consulted the ~40-entry hardcoded table or
  required a `/` in the id.
- `ZoneId.systemDefault()`, called right after
  `TimeZone.setDefault(TimeZone.getTimeZone("CTT"))` (a legacy 3-letter
  `ZoneId.SHORT_IDS` alias), returned UTC instead of `Asia/Shanghai`:
  `ZoneId.of(String)`'s single-arg overload doesn't understand
  `ZoneId.SHORT_IDS` (needs the 2-arg overload with that map), so it threw —
  and the native bypass's `if let Ok(...)` guard silently swallowed that,
  falling through to the hardcoded-UTC bypass.

## Fix

New `native-builtins/src/tzdb.rs` module: parses the real
`${java.home}/lib/tzdb.dat` (the exact binary file and format both
`java.time.zone.ZoneRules` and `sun.util.calendar.ZoneInfoFile` read at
real-JDK startup — inspected via `javap`/`unzip`-ing the JDK's own
`src.zip`) and reimplements `ZoneRules.getOffset(Instant)`'s algorithm
faithfully, ported line-for-line from OpenJDK 25 source
(`ZoneRules.java`, `ZoneOffsetTransitionRule.java`,
`ZoneInfoFile.java`'s binary-format reader). Validated **7248/7248 exact
matches** against real HotSpot JDK 25 across all 604 IANA zones and 12
instants spanning 1900–2100 (via a standalone Rust cross-check harness)
before being wired into the VM.

Wired into `sun.util.calendar.ZoneInfo`/`java.util.SimpleTimeZone`'s
`getOffset`/`getOffsets`/`getOffsetsByWall`/`getRawOffset` (covering both
concrete classes `GregorianCalendar`'s real bytecode dispatches to,
depending on which one `alloc_synth_timezone` constructed for a given zone
id), with a deliberate pre-1900 floor mirroring `ZoneInfoFile`'s own
`UTC1900` cutoff — real HotSpot's *legacy* path doesn't carry pre-1900
history for every zone the way `java.time`'s `ZoneRules` does (confirmed via
the removed `historical_lmt_offset` table's own investigation notes), so
this floor keeps matching HotSpot's legacy quirks bug-for-bug rather than
becoming more textbook-correct than the reference JVM.

Also fixed the two zone-id-resolution bugs above (`TimeZone.getTimeZone`'s
recognized-id check now backed by the real 604-zone tzdb catalog instead of
the ~40-entry table; `ZoneId.systemDefault()` resolves `ZoneId.SHORT_IDS`
aliases through the same catalog before calling `ZoneId.of`).

Removed the now-fully-superseded `tz_dst_rule`/`dst_start_year`/
`historical_lmt_offset` hand-rolled approximation tables.

## Verification

- `org.h2.test.unit.TestTimeStampWithTimeZone` (`testConversions`,
  `testConversionsImpl`) — was failing (6h delta), now **PASSes** across all
  632 zone ids `TimeZone.getAvailableIDs()` returns.
- `org.h2.test.unit.TestValue` (`testTimestamp`, the DST-boundary
  `Calendar.getInstance(TimeZone.getTimeZone("Europe/Berlin"))` case) —
  **PASSes**.
- `org.h2.test.db.TestDateStorage` (the original `bug-h2-timezone-dst-offset.md`
  doc's DST-gap repro) — **PASSes**.
- Broad H2 suite regression pass (`apps/h2database-suite-runner`, jit-real
  mode): no new failures beyond ones already documented elsewhere in
  `../../../known-issues/h2/` (`TestAuthentication`, `TestAlter`,
  `TestLargeBlob`, etc. — pre-existing, unrelated). `TestDate`/
  `TestDateTimeUtils` intermittent hangs under host load confirmed
  **pre-existing on the baseline (pre-fix) binary too** (identical timeout
  behavior with and without this change), so not a regression.

## Related, still-separate finding (unchanged, not touched by this fix)

`org.h2.test.jdbc.TestPreparedStatement.testDate8` — a ~9-day-23-hour delta
on a 1582 date, almost certainly a Julian-vs-proleptic-Gregorian
calendar-system mismatch (`java.time` is always proleptic Gregorian; legacy
`java.util.GregorianCalendar` has a real historical Julian→Gregorian
cutover at 1582-10-15). Explicitly a different class of bug from the
zone-offset miscalculation this doc covered — see
`bug-h2-suite-residual-fail-triage.md`.
