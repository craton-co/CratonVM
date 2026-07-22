# H2 — `java.time`/`TimeZone` zone-rule offset miscalculation (date-time correctness)

## Status
**OPEN** — rediscovery of a previously-documented issue. The original doc
(`docs/known-issues/h2-suite-bugs/bug-h2-timezone-dst-offset.md`) was deleted
by the same `b71e7402f` docs-cleanup commit as the CP500 charset doc, without
a fix landing first — it still reproduces against current `dev` (`origin/dev
@ b21de8501`, 2026-07-21), with a different specific offset than the last
time it was measured (consistent with "still broken", not "coincidentally
the same bug").

## Severity
**MEDIUM-HIGH (correctness)** — any timestamp-with-timezone conversion can
come out wrong by a non-constant, zone/date-dependent number of hours.

## Affected test class (this session)
`org.h2.test.unit.TestTimeStampWithTimeZone` (`testConversionsImpl` /
`testConversions`), PASSes on the HotSpot JDK25 baseline.

## Symptom (2026-07-21 rerun)
```
java.lang.AssertionError:  expected: TIMESTAMP '2017-12-06 11:59:30.987654321'
                             actual: TIMESTAMP '2017-12-06 05:59:30.987654321'
	at org/h2/test/unit/TestTimeStampWithTimeZone.testConversionsImpl(TestTimeStampWithTimeZone.java:193)
```
A **6-hour** delta this run. The original (deleted) doc's last measurement
of the same class showed a **3-hour** delta on a different date/zone
combination, and a companion class (`TestValue`) showed exactly a 1-hour
(3,600,000 ms) delta. The deltas are never a single fixed offset across
different dates/zones — consistent with a genuine zone-rule/transition-table
computation bug, not a constant sign or unit error.

## Root cause (unchanged hypothesis from the original doc, not re-derived from scratch this session)
A difference in CratonVM's `java.time.zone.ZoneRules`/
`java.util.TimeZone.getOffset`/`ZoneOffsetTransition` handling — either the
transition-table data itself, or which rule/offset is picked for a given
instant — relative to the real JDK's `tzdb`-backed implementation. Not
re-investigated in depth this session (out of the fail-triage time budget);
the original doc's suggested next steps still apply:
- Probe `ZoneId.systemDefault()` and `ZoneRules.getOffset(LocalDateTime)` /
  `getTransition(...)` directly (bypassing H2) around the specific instants
  these tests use, compared against the HotSpot baseline.
- Confirm whether CratonVM loads real tzdb transition rules or approximates
  with a fixed/default offset for at least some zones.

## Related, possibly-connected finding (not merged into this doc: different symptom shape)
`org.h2.test.jdbc.TestPreparedStatement.testDate8` also fails this run with a
date discrepancy in the same general area (`java.time`/calendar conversion):
```
Expected: 1582-09-15 00:00:00.000 actual: 1582-09-24 23:00:00.000
```
That is a ~9-day-23-hour delta on a 1582 date — nothing like a DST/zone-offset
shift, and much more consistent with a Julian/proleptic-Gregorian calendar
system mismatch (1582 is the year the real Gregorian calendar was adopted;
`java.time` is always proleptic Gregorian, while `java.util.GregorianCalendar`
historically switches at 1582-10-15). Kept as a **separate, not-yet-root-caused
finding** — see `bug-h2-suite-residual-fail-triage.md` — since folding it into
this doc would overstate confidence that the two share a root cause.

## Repro
```bash
cd apps/h2database/h2
<cratonvm-bin> --java-home /home/victor/jdk25 \
  -c "target/classes:target/test-classes:$(cat craton-testcp.txt)" \
  org.h2.test.unit.TestTimeStampWithTimeZone
```
