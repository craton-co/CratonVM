# `ZonedDateTimeTest` pre-1911 `Europe/Paris` LMT precision — FIXED (2026-07-17)

## Summary

The final 20/608 residual failures in
`org.hibernate.orm.test.type.temporal.ZonedDateTimeTest` (after the
`ZoneId.systemDefault()` host-timezone-leak fix — see
[hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md](hib-zoneddatetime-systemdefault-host-timezone-leak-FIXED.md)
— took it from 63 down to 20) are now fixed. `found=608 started=608
ok=404 failed=0 aborted=204 skipped=0`, matching real HotSpot JDK 25
exactly, stable across 5 independent solo reruns.

**This was not a `java.time`/tzdata precision bug**, despite every
symptom pointing that way at first (the residual doc's original framing
called it a "historical tzdb rule precision" gap). A standalone,
HotSpot-compared micro-repro proved CratonVM's `java.time.ZonedDateTime`/
`ZoneId`/`ZoneRules` arithmetic for the pre-1911 Local Mean Time (LMT)
case is already byte-for-byte correct. The actual bug lived in a
completely separate, legacy API family:
`java.util.TimeZone`/`Calendar`/`SimpleTimeZone`, which
`java.sql.Timestamp`'s deprecated constructor and
`GregorianCalendar.computeTime()`/`.computeFields()` use internally for
the Hibernate JDBC round-trip.

## Background

France used Paris Mean Time (UTC+00:09:21 — 9 minutes 21 seconds) as its
legal civil time until 1911-03-11, when it switched to WET (UTC+0). The
failing test parameters
(`ZonedDateTimeTest.testData()`'s final six `.add(1904, 12, 31, 23, 59,
59, 999_999_999, ...)`/`.add(1905, 1, 1, 0, 0, 0, 0, ...)` entries, all
with `ZONE_PARIS` as the default timezone) exercise exactly this
boundary — the test's own source comment calls it out: "Also test dates
around 1905-01-01, because the code behaves differently before and after
1905."

## Investigation

A standalone Java micro-repro (`ParisLmtRepro.java`), constructing
`ZonedDateTime.of(1904/1905, ..., ZoneOffset)` and converting via
`.withZoneSameInstant(ZoneId.of("Europe/Paris"))` — the same operation
the test's `getExpectedPropertyValueAfterHibernateRead()` performs —
produced **identical, byte-for-byte output** on real HotSpot JDK 25 and
CratonVM (both real-JDK mode):

```
withZoneSameInstant = 1905-01-01T01:09:20.999999999+00:09:21[Europe/Paris]
```

This ruled out `java.time`/`ZoneRules` entirely and pointed at the
Hibernate/JDBC layer instead — specifically
`getExpectedJdbcValueAfterHibernateWrite()`, which converts to
`LocalDateTime` then constructs a `java.sql.Timestamp` via its
**deprecated** `Timestamp(year, month, date, hrs, min, sec, nanos)`
constructor. That constructor (via `java.util.Date`'s legacy
epoch-computation path) is timezone-sensitive, using whatever
`TimeZone.getDefault()` is set to at construction time — a completely
different code path from `java.time`.

A second micro-repro (`ParisLegacyRepro.java`), directly exercising
`TimeZone.getOffset(long)`, the deprecated `Timestamp` constructor, and
`GregorianCalendar.getTimeInMillis()` (bypassing Hibernate entirely),
confirmed the bug: for `1905-01-01T00:00:00Z`, CratonVM's
`TimeZone.getOffset(millis)` returned `3600000` ms (a flat, modern
+1:00 CET offset) instead of the correct `561000` ms (+00:09:21 LMT) —
real HotSpot returned `561000` ms. The resulting epoch-millis
computation was off by exactly `3600 - 561 = 3039` seconds, matching the
skew visible in the original failure signatures precisely:

```
expected: <1905-01-01T01:09:20+00:09:21[Europe/Paris]> but was: <1905-01-01T00:18:41+00:09:21[Europe/Paris]>
expected: <1905-01-01 01:09:21.0> but was: <1905-01-01 02:00:00.0>
```

(Both sides show the *correct* `+00:09:21` offset in the `ZonedDateTime`
zone/offset field, because that display value comes from `java.time`'s
correct `ZoneRules` — the bug is purely in the legacy Calendar-arithmetic
instant computed during the JDBC round-trip, which is why the offset
"looks right" but the local time doesn't.)

## Root cause

CratonVM's real-JDK-mode `java.util.TimeZone` is not backed by real
tzdata: `sun.util.calendar.ZoneInfoFile`'s `<clinit>` fails to load
`${java.home}/lib/tzdb.dat` in this VM (an old, pre-existing, unrelated
bootstrap gap — see the `HIB-CV-34` comments in
`native-builtins/src/lib.rs`). As a workaround,
`TimeZone.getTimeZone(String)`/`getDefault()` are natively overridden
(`alloc_synth_timezone`) to synthesize a `TimeZone` instance from a
small, hand-rolled table instead of real tzdata:

- Zones with a currently-known annual DST rule (`tz_dst_rule` — all the
  `Europe/*` zones this suite exercises, via the "EU rule": DST from the
  last Sunday of March to the last Sunday of October) get a **real
  `java.util.SimpleTimeZone`**, constructed with `rawOffset` set to the
  zone's *modern* standard offset (e.g. +1:00 for `Europe/Paris`) plus
  that DST rule.
- Zones without a known DST rule get a synthetic
  `sun.util.calendar.ZoneInfo` with `rawOffset` from the same modern
  table and `transitions` left `null` (flat offset year-round).

Both representations are structurally "year-round-constant plus, at
best, one recurring annual rule" — neither has any way to encode a
**one-time historical cutover** like Paris's 1911-03-11 LMT→WET switch.
Concretely, cross-referencing `$JAVA_HOME/lib/src.zip`'s
`GregorianCalendar.java`/`SimpleTimeZone.java`:

- `GregorianCalendar.computeTime()`/`.computeFields()` call
  `SimpleTimeZone.getOffsets(long, int[])` (a package-private method) for
  any zone that isn't a `sun.util.calendar.ZoneInfo` — i.e. every
  `Europe/*` zone this suite touches, since they all take the
  `SimpleTimeZone` branch above.
- `SimpleTimeZone.getOffsets(long, int[])` only ever consults the
  `rawOffset` field (a plain `int`, fixed at construction — **no date
  parameter**) plus the DST-rule check. There is no structural way to
  make `rawOffset` itself date-dependent through this API.
- So for any pre-1911 `Europe/Paris` instant, the legacy path always
  returned the modern +1:00 (`3600` s) offset instead of the correct
  +00:09:21 (`561` s) LMT offset.

This explains why `java.time` (a fully separate implementation, backed
by the real JDK's own `ZoneRulesProvider`/tzdb text data, never
intercepted by any of these natives) was already exactly right, while
the JDBC/legacy-Calendar half was not: two independent time subsystems,
only one of which had ever been extended with a synthetic tzdata table,
and that table only ever modeled the *modern* offset.

## Fix

`native-builtins/src/lib.rs`, `register_essential_natives`:

1. Added `historical_lmt_offset(zone_id) -> Option<(cutover_epoch_millis,
   pre_cutover_offset_seconds)>` — a small, explicit table (deliberately
   not full historical tzdata) giving the exact pre-standardization LMT
   cutover for `Europe/Paris` only: `(-1855958961_000, 561)`
   (1911-03-10T23:50:39Z, the exact UTC instant of Paris's
   1911-03-11T00:00 local switch to WET, cross-checked against real
   HotSpot JDK 25's `ZoneId.of("Europe/Paris").getRules()
   .getTransitions()`).
2. Registered a native override for `java.util.SimpleTimeZone`'s
   package-private `getOffsets(long, int[])` — the single method both
   `GregorianCalendar.computeTime()` (write direction) and
   `.computeFields()` (read direction, via `SimpleTimeZone.getOffset(long)`,
   which itself just calls `getOffsets(date, null)`) route through. For
   any queried instant strictly before the zone's cutover, it returns the
   historical LMT offset directly; otherwise it defers to the **real**
   `SimpleTimeZone` bytecode via `ctx.invoke_virtual_bytecode_only(...)`
   (skipping the native-override re-entry check so this doesn't recurse
   into itself), preserving the exact existing (already-correct)
   rawOffset+DST-rule behavior for every date this fix doesn't touch.

### Why not Amsterdam/Oslo/Auckland too?

Real IANA tzdata (and CratonVM's/HotSpot's `java.time.ZoneRules`, which
reads it) also models pre-standardization LMT cutovers for
`Europe/Amsterdam` (+00:17:30 until 1892-05-01), `Europe/Oslo`
(+00:53:28 until 1893-03-31), and `Pacific/Auckland` (+11:39:04 until
1868-11-02) — all zones this codebase's `tz_dst_rule`/`alloc_synth_timezone`
also constructs as `SimpleTimeZone`. The obvious generalization would add
table entries for all of them.

A `GeneralityRepro.java` probe (comparing `TimeZone.getOffset(long)` and
`GregorianCalendar` results against real HotSpot JDK 25, for dates just
before each zone's cutover) showed this would be a **mistake**: unlike
Paris, real HotSpot's own *legacy* `TimeZone`/`GregorianCalendar` path
does **not** correctly resolve the Amsterdam/Oslo pre-cutover LMT offset
either — it returns the flat modern offset (`3600` s), the same wrong
answer CratonVM produced pre-fix. Only `java.time`'s `ZoneRules` gets
these two right on real HotSpot; the legacy path's compiled zoneinfo
data apparently omits (or never carried) these particular pre-1892/1893
rules, even though it does carry Paris's. Adding table entries for
Amsterdam/Oslo would therefore make CratonVM's legacy `Calendar` path
**more textbook-correct than real HotSpot itself** for those two zones —
i.e. it would *diverge* from the reference JVM this project targets
bug-for-bug compatibility with, not converge on it. `Pacific/Auckland`
was not separately probed but is presumed to carry the same risk and was
left out for the same reason.

The fix mechanism itself (table + `getOffsets` override +
bytecode-fallback) is fully general — it would take one line to add a
zone once a genuine HotSpot-matching need is identified and verified the
same way. See the code comment on `historical_lmt_offset` for the
verification method to use before adding an entry.

### Related, out-of-scope finding

While probing generality, a separate, unrelated, pre-existing bug was
found: CratonVM's synthetic `SimpleTimeZone` construction retroactively
applies the *modern* EU DST rule to 19th-century dates that predate real
DST adoption in that region. `Europe/Amsterdam` at `1892-04-01T00:00:00Z`
returns `+2:00` (DST "on") on CratonVM vs. real HotSpot's `+1:00` (DST
correctly modeled as not-yet-existing — the Netherlands didn't adopt DST
until 1916). Not investigated or fixed here (not blocking any known
test); flagged for a future session, e.g. via `SimpleTimeZone.setStartYear()`.

## Verification

- `ParisLegacyRepro.java` (standalone, no Hibernate): post-fix,
  `TimeZone.getOffset(1905-01-01T00:00:00Z)` = `561000` ms,
  `Timestamp.getTime()` = `-2051218800000` ms, `GregorianCalendar
  .getTimeInMillis()` = `-2051218800000` ms — all three now match real
  HotSpot exactly (pre-fix: `3600000`/`-2051221839000`/`-2051221839000`).
- `ParisLmtRepro.java` (`java.time`-only): unchanged pre/post fix, as
  expected — the fix never touches `java.time`.
- `ZonedDateTimeTest`: `found=608 started=608 ok=404 failed=0
  aborted=204 skipped=0`, matching real HotSpot's exact shape, stable
  across 5 independent solo reruns (`--nojit`, real-JDK, both pre-merge
  and post-merge onto `dev`).
- `LocalDateTimeTest`: `found=162 ok=90 failed=0 aborted=72` — no
  regression, re-verified against the merged-`dev` tip.
- `InstantTests`: `found=204 ok=112 failed=0 aborted=92` — no
  regression, re-verified against the merged-`dev` tip.
- The intermittent `InternalError: CloneNotSupportedException` flagged
  in the residuals doc's prior update (observed ~1-in-4 reruns
  pre-fix) did not reproduce in any of the 5 post-fix reruns.

## Commit

Branch `fix/hib-paris-lmt-precision-20260717`, commit `31544c27`
(`native-builtins/src/lib.rs`), merged to `dev` at `03d7e98f`.
