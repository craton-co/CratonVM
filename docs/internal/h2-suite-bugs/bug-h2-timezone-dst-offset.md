# H2 — timezone / DST offset miscalculation (date-time correctness)

## Status
**FIXED** (2026-07-22) — see
`bug-h2-timezone-zonerules-offset-miscalculation-FIXED.md` in this same
directory for the full root-cause/fix writeup (this doc was rediscovered
independently as `docs/known-issues/h2-suite-bugs/bug-h2-timezone-zonerules-offset-miscalculation.md`
before either doc's relationship to the other was noticed; both cover the
exact same bug — this doc's three symptoms below, `TestDateStorage`'s
8-hour DST-gap shift and `TestValue`'s 1-hour delta included, all verified
fixed alongside `TestTimeStampWithTimeZone`'s 3h/6h deltas).

## Severity
**MEDIUM-HIGH (correctness)** — wrong timestamps for any zone-aware conversion.

## Affected test classes (mem config)
| Class | Expected | Actual | Delta |
|-------|----------|--------|-------|
| `TestDateStorage` | `2010-03-14 02:15:00` | `2010-03-13 18:15:00` | −8 h (across a day) |
| `TestTimeStampWithTimeZone` | `2017-12-06 11:59:30.987654321` | `2017-12-06 14:59:30.987654321` | +3 h |
| `TestValue` | `1521943140123` | `1521939540123` | +3 600 000 ms (exactly 1 h) |

## HotSpot behavior
PASS — all three produce the expected zone-converted values.

## Analysis
The deltas are not a single fixed offset (8 h / 3 h / 1 h), so this is **zone-rule
/ DST offset computation**, not a constant-offset slip:

- `TestDateStorage` expects `2010-03-14 02:15:00`. **2010-03-14 is the US DST
  spring-forward date** (local clocks jump 02:00→03:00, so 02:15 is in the gap).
  CratonVM yields `2010-03-13 18:15:00` — an 8-hour shift that crosses the day
  boundary, i.e. the DST transition / wall-clock-in-gap resolution differs from
  the JDK.
- `TestValue`'s delta is exactly **3 600 000 ms = 1 hour**, the classic
  DST-offset-applied-or-not discrepancy.

Likely a difference in CratonVM's `java.time.zone.ZoneRules` /
`java.util.TimeZone.getOffset` / `ZoneOffsetTransition` handling (transition
tables, gap/overlap resolution, or the default zone the VM reports).

## Next steps
- Probe `ZoneId.systemDefault()` and
  `ZoneRules.getOffset(LocalDateTime)` / `getTransition(...)` around
  `2010-03-14T02:15` and a known DST boundary on CratonVM vs HotSpot.
- Check whether CratonVM loads the tzdb (`java.time.zone.TzdbZoneRulesProvider`)
  transition rules or falls back to a fixed-offset approximation.
- Confirm the VM's default timezone matches the host (the −8 h hints at a
  US/Pacific-vs-UTC default-zone or offset-sign issue).

## Repro
`TestDateStorage`, `TestTimeStampWithTimeZone`, `TestValue`, or a minimal
`ZonedDateTime.of(2010,3,14,2,15,0,0, ZoneId.of("America/Los_Angeles"))`
round-trip compared against HotSpot.
