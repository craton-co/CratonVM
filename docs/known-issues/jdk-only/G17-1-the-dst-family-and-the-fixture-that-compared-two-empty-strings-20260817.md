# G17-1 — the DST family answered "no zone has ever observed daylight saving", and the fixture that would have said so compared two empty strings

> **RECONCILED 2026-08-17 (lane G40) — one inference weakened; the record's
> headline is CONFIRMED and its vector is now GREEN.**
>
> * **Confirmed and closed.** `RSimpleTimeZoneRaw` passes, **MEASURED** on the
>   `9964ca733` binary under `--jdk-only`. Three pieces had to land first, in
>   three commits by three lanes: this record's fixture rewrite (the old fixture
>   compared two empty strings and could not fail — it went from 0 surviving
>   `extract()` lines to 469), the `rawOffset` field seeding, and the DST rule
>   layer of `G28-1` (`93cd4e8da`). The rule layer's author predicted 74
>   divergences → 0 without ever being able to build, and it held exactly.
> * **Weakened.** §1.3 and §4 argue that registering narrowly on `ZoneInfo`
>   "loses nothing measured" because *"the base's rows read `invocations=0`"*.
>   `G33-1` established that `invocations` is a **floor** — it counts only
>   registry-resolved dispatches, so `invocations == 0` proves nothing. The
>   narrow registration is not shown to be wrong by this; the *argument* for it
>   no longer stands on its own, and §4's own hypothetical (an application's
>   `extends TimeZone` subclass) is exactly the case a zero cannot rule out. See
>   `INDEX.md` §B.1.

**Status:** BEFORE **MEASURED** on both VMs; the RULE the fix implements
**MEASURED** against the oracle on 9,480 rows / 632 zone ids, 0 mismatches; the
Rust "after" is **UNMEASURED — needs a build** (see §8). The corrected fixture
is **MEASURED on both VMs, before and after**.
**Provenance:** MEAS on both VMs unless a row says otherwise. Oracle is HotSpot
25.0.3+9-LTS at `$JAVA_HOME`. CratonVM binary is
`C:/craton/target-fcheck/release/cratonvm.exe`, run `--jdk-only`.
Probes: `scratchpad/g17/{G17Probe,G17Edge,G17All,G17Model,RSimpleTimeZoneRaw}.java`.

Takes the handover from `C12-1-simpletimezone-the-trap-and-the-second-site.md`,
whose two corrections this record does not touch and does not re-litigate: the
São Paulo skew is **7,200,000 ms, measured**, and **the skew is not a
constant**. `P4A-CORPORA-20260812`'s `10800000` is still a different, correct
quantity.

---

## 0. The headline

| thing | before (MEASURED) | after |
|---|---|---|
| `RSimpleTimeZoneRaw` on CratonVM | `AssertionError: New_York inDaylightTime(JUL)`, **stdout empty** | red for 42 named rows, **counts published** (corrected fixture, MEASURED) |
| what survives `extract()` on the CratonVM side | **0 lines** — G2 and G3 both fire | 273 lines including 271 observables (corrected fixture, MEASURED) |
| `java.util.TimeZone` DST family, all 632 available ids | **542 of 632 ids wrong** | rule validated 9,480/9,480 against the oracle; VM re-run pending a build |
| six-arg `getOffset(era,y,m,d,dow,ms)` | **493 of 632 ids wrong**, 486 of them answering literally `0` | same |

The first failing assertion was one row. The family behind it was
`getDSTSavings()`, `useDaylightTime()`, `observesDaylightTime()`,
`inDaylightTime(Date)` and the six-arg `getOffset` — five methods, none of them
registered, all of them answering from a `sun.util.calendar.ZoneInfo` whose
`transitions` array is null and whose `rawOffset` field was written by a
31-arm hand table that was retired for everything except that one write.

---

## 1. Which body runs — settled from the registry, not from reading

`--dump-native-registry` **before** the main class (flags after it are ignored
silently, exit 0, no file). Every row for the zone family, from one run of
`G17Probe`:

```
sun/util/calendar/ZoneInfo getOffset        (J)I    owns_slot=true inv=358 by=lib.rs:20594 overwrote=null
sun/util/calendar/ZoneInfo getRawOffset     ()I     owns_slot=true inv=10  by=lib.rs:20657 overwrote=null
sun/util/calendar/ZoneInfo getOffsets       (J[I)I  owns_slot=true inv=0   by=lib.rs:20606 overwrote=null
sun/util/calendar/ZoneInfo getOffsetsByWall (J[I)I  owns_slot=true inv=0   by=lib.rs:20623 overwrote=null
java/util/TimeZone         getOffset        (J)I    owns_slot=true inv=0   by=lib.rs:20594 overwrote=null
java/util/TimeZone         getRawOffset     ()I     owns_slot=true inv=0   by=lib.rs:20657 overwrote=null
java/util/TimeZone         getID   ()Ljava/lang/String; owns_slot=true inv=9 by=lib.rs:20719 overwrote=null
```

Three facts follow, and each answers a question that reading could not:

1. **`ZoneInfo.getOffset(J)I` is the body that runs** — `owns_slot=true` with
   `invocations=358`. C12-1 §2's source reading is confirmed by execution: the
   `SimpleTimeZone` unregistration left the `ZoneInfo` family answering and did
   not disturb it.
2. **`getDSTSavings`, `useDaylightTime`, `observesDaylightTime`,
   `inDaylightTime` and `getOffset(IIIIII)I` appear in NO row, on either
   class.** A grep of the whole tree for those method names as registration
   arguments returns nothing. They ran real JDK bytecode, which is exactly the
   problem: the object they ran it against is fabricated.
3. **`java/util/TimeZone`'s copies have `invocations=0`.** The base
   registration has no constituency in real-JDK mode; it exists for the
   `--synthetic-jdk` stub. That is why §4 registers on `ZoneInfo` only.

**The duplicate-registrar shape a sibling lane found elsewhere does NOT occur
here.** `register_tzdb_offset_natives_for` is defined once
(`lib.rs:20590`); `register_time_natives` is defined once
(`util_time.rs:226`); every row above reads `overwrote=null`. There is no
second registrar of the same name in another file silently winning.

**What DOES occur here is the same shape one level down** — two producers of
one quantity, the stale one still writing. See §3.

## 2. The oracle table

`TimeZone.getTimeZone(id)`, fixed ids only, no host default read anywhere.
`JAN = 1610712000000` (2021-01-15T12:00:00Z), `JUL = 1626350400000`
(2021-07-15T12:00:00Z), six-arg at `AD, 2021, <month>, 15, <dow>, 43200000`.
Every HotSpot value below was executed on this host; every CratonVM value was
executed on the binary above.

| zone | accessor | HotSpot | CratonVM (before) |
|---|---|---|---|
| `America/New_York` | `getRawOffset` | `-18000000` | `-18000000` |
| | `getDSTSavings` | `3600000` | **`0`** |
| | `useDaylightTime` | `true` | **`false`** |
| | `observesDaylightTime` | `true` | **`false`** |
| | `getOffset(JAN)` | `-18000000` | `-18000000` |
| | `getOffset(JUL)` | `-14400000` | `-14400000` |
| | `inDaylightTime(JAN)` | `false` | `false` |
| | `inDaylightTime(JUL)` | `true` | **`false`** |
| | six-arg JAN | `-18000000` | `-18000000` |
| | six-arg JUL | `-14400000` | **`-18000000`** |
| `Europe/London` | `getDSTSavings` | `3600000` | **`0`** |
| | `useDaylightTime` / `observesDaylightTime` | `true` / `true` | **`false`** / **`false`** |
| | `inDaylightTime(JUL)` | `true` | **`false`** |
| | six-arg JUL | `3600000` | **`0`** |
| `Australia/Lord_Howe` | `getRawOffset` | `37800000` | `37800000` |
| | `getDSTSavings` | **`1800000`** | **`0`** |
| | `useDaylightTime` | `true` | **`false`** |
| | `getOffset(JAN)` / `(JUL)` | `39600000` / `37800000` | `39600000` / `37800000` |
| | `inDaylightTime(JAN)` | `true` | **`false`** |
| | six-arg JAN / JUL | `39600000` / `37800000` | **`0`** / **`0`** |
| `Pacific/Chatham` | `getRawOffset` | `45900000` | `45900000` |
| | `getDSTSavings` | `3600000` | **`0`** |
| | `getOffset(JAN)` / `(JUL)` | `49500000` / `45900000` | `49500000` / `45900000` |
| | `inDaylightTime(JAN)` | `true` | **`false`** |
| | six-arg JAN / JUL | `49500000` / `45900000` | **`0`** / **`0`** |
| `Asia/Kolkata` | `getRawOffset` | `19800000` | `19800000` |
| | `getDSTSavings` / `useDaylightTime` | `0` / `false` | `0` / `false` |
| | six-arg JAN / JUL | `19800000` / `19800000` | `19800000` / `19800000` |
| `America/Sao_Paulo` | `getRawOffset` | `-10800000` | `-10800000` |
| | `getDSTSavings` | **`0`** | `0` |
| | `useDaylightTime` / `observesDaylightTime` | `false` / `false` | `false` / `false` |
| | six-arg JAN / JUL | `-10800000` / `-10800000` | `-10800000` / `-10800000` |
| `UTC` | all of the above | `0` / `false` | `0` / `false` |
| `GMT+05:30` (fixed) | `getRawOffset` | `19800000` | `19800000` |
| | `useDaylightTime` / `observesDaylightTime` | `false` / `false` | `false` / `false` |
| `Nowhere/Nozone` (unknown) | `getID` | `GMT` | `GMT` |
| | `getRawOffset` / `useDaylightTime` | `0` / `false` | `0` / `false` |

Boundaries — one millisecond either side of a real transition, both
hemispheres. HotSpot column; CratonVM matched every `getOffset` and **inverted
every `inDaylightTime` that should be `true`**:

| zone | transition (epoch ms) | `getOffset` before / at | `inDaylightTime` before / at (HotSpot) |
|---|---|---|---|
| `America/New_York` | `1615705200000` | `-18000000` / `-14400000` | `false` / `true` |
| `America/New_York` | `1636264800000` | `-14400000` / `-18000000` | `true` / `false` |
| `Europe/London` | `1616893200000` | `0` / `3600000` | `false` / `true` |
| `Europe/London` | `1635642000000` | `3600000` / `0` | `true` / `false` |
| `Australia/Lord_Howe` | `1617462000000` | `39600000` / `37800000` | `true` / `false` |
| `Australia/Lord_Howe` | `1633188600000` | `37800000` / `39600000` | `false` / `true` |
| `Pacific/Chatham` | `1617458400000` | `49500000` / `45900000` | `true` / `false` |
| `Pacific/Chatham` | `1632578400000` | `45900000` / `49500000` | `false` / `true` |

Argument contracts, MEASURED on both VMs. **These were already right** and are
recorded because the fix must not lose them:

| call | HotSpot | CratonVM (before) |
|---|---|---|
| `ZoneInfo.inDaylightTime(null)` | `java.lang.NullPointerException`, message **`null`** | same |
| six-arg, `millis = -1` | `IllegalArgumentException`, message **`null`** | same |
| six-arg, `millis = 86400000` | `IllegalArgumentException`, message `null` | same |
| six-arg, `era = 7` | `IllegalArgumentException`, message `null` | same |
| six-arg, `era = BC, year = 100` | `-18000000` | same |
| six-arg, `dayOfWeek = MONDAY` for a Thursday | `-14400000` | **`-18000000`** (the dow is ignored on both; this row is the DST defect) |
| `SimpleTimeZone.inDaylightTime(null)` | `NPE`, message `Cannot invoke "java.util.Date.getTime()" because "date" is null` | same |

Two of those messages **cannot be derived, only transcribed**: `ZoneInfo`'s NPE
has a *null* message where `SimpleTimeZone`'s has a helpful one, and all three
`IllegalArgumentException`s are message-less. `RuntimeError::NullPointerException
{ message: None }` and `RuntimeError::IllegalArgumentException { message:
String::new() }` are the two spellings that reproduce that; anything else
produces `""` and diverges.

**The whole-catalog census.** `G17All` asks eleven accessors of every id
`TimeZone.getAvailableIDs()` returns:

```
zones=632   rows differing=542
  s6Jul 532   s6Jan 493   obs 217   dst 214   use 214   iJul 197   iJan 37
  raw    0    oJan   0    oJul   0
```

`getRawOffset` and `getOffset(long)` — the two members that WERE registered —
are correct on all 632. Every other member is wrong on a third to five-sixths
of the catalog.

**`use` 214 vs `obs` 217.** Three zones separate them:
`Africa/Casablanca`, `Africa/El_Aaiun`, `Africa/Windhoek`, all
`use=false obs=true`, because tzdb models their permanent shift as a *standing
daylight offset* with no recurring rule. A rule for `observesDaylightTime`
keyed only on `lastRules` gets those three wrong. `useDaylightTime() ||
inDaylightTime(now)` — the documented `java.util.TimeZone` contract — gets all
632 right, and is what §4 implements. It is the one member of the family whose
answer legitimately depends on when you ask.

## 3. Why — and the second site, which is a FIELD and not a registration

`alloc_synth_timezone` (`lib.rs:20208`) builds the `ZoneInfo` that
`TimeZone.getTimeZone(id)` returns. Its own comment states the consequence:

> `transitions` stays null, so `ZoneInfo.getOffset(long)` returns this
> rawOffset for all instants.

`getOffset(long)` is registered, so that sentence is no longer true of it. It
is still true of every member that is **not** registered, and `ZoneInfo`'s
bytecode all keys on the same null array:

* `inDaylightTime(Date)` → `transitions == null` → `false`, at every instant,
  for every zone.
* `useDaylightTime()` / `observesDaylightTime()` → no `simpleTimeZoneParams`
  → `false`.
* `getDSTSavings()` → the `dstSavings` field, which `alloc_synth_timezone`
  writes as a literal `Value::Int(0)`.

**The six-arg `getOffset` is the second site, and it is a different mistake.**
It reads the `rawOffset` FIELD, and that field is seeded from

```rust
let raw_offset_ms = tz_standard_offset_seconds(id_str).unwrap_or(0).saturating_mul(1000);
```

`tz_standard_offset_seconds` (`lib.rs:19914`) is a **31-arm hand table**. The
TZDB-OFFSET note eleven lines below it says the tzdb path *"Supersedes the
previous … hand-rolled approximations"* — and it does, for every reader that
goes through a native. This one write is the reader that does not. Measured:

```
zones=632   six-arg JAN differs on 493
            of which CratonVM answered literally 0 on 486
```

486 of 632 ids have a `rawOffset` field of `0` while `getRawOffset()` — the
native, reading tzdb — answers correctly. **Two producers of one quantity, and
the retired one is still writing the field.** That is C12-1's shape (a second
site the unregistration cannot reach) at the field level rather than the
dispatch level. It is NOMINATED in §7; §4 stops the six-arg reading the field
at all, but any *other* JDK bytecode that reads it stays wrong until the
nomination lands.

## 4. What this lane changed

Two files, both this lane's.

**`native-builtins/src/tzdb.rs`** — the rules, and the five natives.

Rule level (pure, `NativeContext`-free, unit-tested against real `tzdb.dat`):

* `dst_savings_seconds(rules)` — the max of `offset_after - standard_offset`
  over `last_rules`, and **nothing from the transition table**. The max rather
  than the first because a zone's two rules describe the two ends of one
  period. This is the reason `America/Sao_Paulo` answers `0`: dense DST
  history, empty `lastRules`.
* `uses_daylight_time(rules)` — `dst_savings_seconds != 0`, which is
  `ZoneInfo`'s `simpleTimeZoneParams != null`.
* `legacy_offsets_ms_of(rules, ms)` — the existing legacy-floor rule, split out
  of the `ctx`-taking `legacy_offsets_ms` so it is testable. Behaviour
  unchanged; the unknown-id arm still answers `(0, 0)`.
* `offset_ms_at_local_standard_of(rules, local_ms)` — the six-arg's rule.
  Guess-and-refine on the **standard** offset, not the total: the input is
  standard local time, so daylight saving must not enter the conversion from
  wall reading to instant, only the answer read off at the resulting instant.
* `observes_daylight_time(ctx, id, now)` — `uses || inDaylightTime(now)`.

`register_zoneinfo_dst_natives(registry)` registers exactly five triples, on
`sun/util/calendar/ZoneInfo` **and on nothing else**:

```
sun/util/calendar/ZoneInfo getDSTSavings        ()I
sun/util/calendar/ZoneInfo useDaylightTime      ()Z
sun/util/calendar/ZoneInfo observesDaylightTime ()Z
sun/util/calendar/ZoneInfo inDaylightTime       (Ljava/util/Date;)Z
sun/util/calendar/ZoneInfo getOffset            (IIIIII)I
```

**Not on `java/util/TimeZone`, deliberately**, though the sibling family in
`lib.rs` is. `TimeZone.getDSTSavings()` and `TimeZone.observesDaylightTime()`
are **concrete** on the base, so an application's own `extends TimeZone`
subclass that does not override them would have no bytecode of its own, the
superclass climb would run, and it would be answered from a tzdb lookup of its
`ID`. That is precisely the C6-2 defect in a new place. The base's rows read
`invocations=0` in §1, so the narrow registration loses nothing measured.
(`java.util.SimpleTimeZone` declares all five itself and could not have been
captured either — but it does not need to be argued about while nothing is on
the base.)

`inDaylightTime` reaches the instant through a real
`ctx.invoke_virtual(date, "getTime", "()J")`, pinning both references across it,
because a deprecated `Date` setter can leave `cdate` dirty and force a
normalise — the same reason `phases_late/ssl_security.rs` does it that way for
`checkValidity(Date)`. Slot 0 is the fallback for a synthetic `Date`.

**`native-builtins/src/date_format_fast.rs`** — one call, and the reason it is
there.

`register_date_format_fast` gains `crate::tzdb::register_zoneinfo_dst_natives(r)`.
It is a strange address for it and the comment says so. The constraint that
produced it: **`util_time.rs` — the obvious home, where the `java.time` doors
live — is `#[cfg(feature = "synthetic-jdk")]` and is not compiled into the
default build at all** (`lib.rs:4170`). Its own module doc says so: *"not one"*
of the shipped build's 11,665 registrations names that file. A registration
added there would be dead code that reads as a fix — the trap
`HANDOFF-20260814` §5 records twice. `register_date_format_fast` is called from
the real-JDK arm of `register_essential_natives_with_shims`, after the phase
registrations, which is the property that was actually needed. Relocation next
to `register_tzdb_offset_natives_for` is NOMINATED in §7.

That the chosen site is reached is **measured, not assumed**: the same
`--dump-native-registry` run as §1 carries

```
java/text/DateFormat format (Ljava/util/Date;)Ljava/lang/String; owns_slot=true by=date_format_fast.rs:953
```

so `register_date_format_fast` executes under `--jdk-only` and the five new
triples will be in the registry. `util_time.rs` appears in **no** row of that
dump, which is the same fact stated the other way round.

**Unit tests**, in `tzdb.rs`'s existing `#[cfg(test)] mod tests`, all against
the real `tzdb.dat` the test module already loads, all with values transcribed
from the oracle: `dst_savings_matches_hotspot_get_dst_savings` (7 zones,
including the 30-minute and the abolished-DST cases),
`uses_daylight_time_matches_hotspot` (9 zones, including the three
`use=false obs=true` ones), `in_daylight_time_matches_hotspot_at_the_fixture_instants`,
`in_daylight_time_is_exact_at_a_transition_boundary` (±1 ms, both directions),
`six_arg_offset_reads_local_standard_time` (8 rows, including both half-hour
zones), `six_arg_offset_honours_the_1900_floor`,
`gregorian_epoch_day_anchors_and_normalises`, and
`unknown_zone_id_answers_utc_not_a_panic`.

## 5. The rule is MEASURED, on 632 zones, without a build

`scratchpad/g17/G17Model.java` computes every quantity in §4 **only from
`java.time.zone.ZoneRules`** — the same `standardTransitions` /
`savingsInstantTransitions` / `wallOffsets` / `lastRules` that `tzdb.rs` parses
out of `tzdb.dat` — and compares each against `java.util.TimeZone`'s own answer,
on HotSpot, for every available id:

```
ZONES=632  SKIPPED=0  ROWS=9480  MISMATCHES=0
```

That is not the Rust running. It is the *rule* the Rust implements, checked
against the oracle on eleven accessors × 632 zones, and it makes the remaining
risk a transcription risk rather than a modelling one.

## 6. The fixture — the blindness is REAL, and it is not only the DST rows

**MEASURED.** With the vector as it stands, CratonVM's side of the cross-VM
diff is empty:

```
$ cratonvm --jdk-only -cp regression-suite/build RSimpleTimeZoneRaw | grep -aE '^(PASS|CK) '
$ (nothing)

HARNESS ERROR [G2] RSimpleTimeZoneRaw: nothing survives extract() -- the
  cross-VM diff compares two empty strings
HARNESS ERROR [G3] RSimpleTimeZoneRaw: publishes no check count, and is not in
  regression-suite/harness-uncounted.txt
```

The fixture throws at its first failing assertion, so on a red run it prints
nothing at all — not the evidence, not the count, not the banner. G2 and G3 are
firing on a genuinely blind run.

**And fixing the DST bug alone would not have fixed that**, which is the point
worth writing down. On the green path the old fixture prints exactly two lines,
both constants:

```
CK RSimpleTimeZoneRaw checks=104
PASS RSimpleTimeZoneRaw (104 checks)
```

G2 accepts a published count as an observable, so it goes quiet — but the
cross-VM diff is then a comparison of the literal `104` against the literal
`104`. **Every one of the 104 assertions is a hardcoded expectation compared
inside the fixture; not one VM answer reaches the diff.** That is the shape
`harness-guard.sh`'s own dialect contract names: *"a fixture that prints only
its own verdict has made the comparison unlosable"*. It is the same defect
`D3-1-simpledateformat-format-zone-arm.md` records for its zone fixture.

### The corrected vector — MEASURED on both VMs

The change is one idea: **every assertion publishes the observed value on its
own `CK` line before comparing it**, and a failing assertion records the
failure instead of aborting, so a red run still publishes its evidence and both
counts. Then the DST family is added, with the values transcribed in §2.

Measured, with the corrected fixture compiled into `scratchpad/g17/vec`:

| | HotSpot | CratonVM (pre-fix binary) |
|---|---|---|
| lines surviving `extract()` | **274** | **273** |
| lines on a prefix `extract()` deletes | **0** (G1 silent) | 0 |
| `checks=` | `271` | `271` |
| `fails=` | `0` | **`42`** |
| banner | `PASS RSimpleTimeZoneRaw (271 checks)` | absent |
| cross-VM diff rows | — | **44** |

The 42 red rows name themselves in the diff:
`dstf.dst[America/New_York]`, `dstf.use[Europe/London]`,
`dstf.sixJan[Pacific/Chatham]`, `edge.lhi.spring.in@0`, `six.lhi.oct`, … The
old fixture said `AssertionError: New_York inDaylightTime(JUL)` and stopped;
this one says which forty-two things diverged and by how much. **229 of the 271
rows match**, including every `SimpleTimeZone` opaque-label row, every
`getOffset(long)` row, `Asia/Kolkata`, `America/Sao_Paulo`, `GMT+05:30`, the
unknown id, and all seven argument-contract rows — so it is a discriminator and
not a fixture that is simply red everywhere.

**NOMINATED** — `regression-suite/src/RSimpleTimeZoneRaw.java`, which is not
this lane's file. Full replacement:

```java
import java.util.Calendar;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.SimpleTimeZone;
import java.util.TimeZone;

/**
 * {@code java.util.SimpleTimeZone}: the {@code ID} argument is an OPAQUE LABEL
 * -- and {@code java.util.TimeZone}'s DAYLIGHT-SAVING family, which is the
 * other half of the same surface and was silently answering "no zone on earth
 * has ever observed daylight saving".
 *
 * WHY THIS EXISTS. CratonVM registers natives on the concrete
 * {@code TimeZone} classes -- {@code getRawOffset()}, {@code getOffset(J)},
 * {@code getOffsets(J[I)} and {@code getOffsetsByWall(J[I)}
 * (native-builtins/src/lib.rs, {@code register_tzdb_offset_natives_for}) -- and
 * every one of them answers by reading the receiver's {@code ID} field and
 * resolving it against tzdb. That is right for a {@code ZoneInfo}, whose id IS
 * the zone. It is wrong for a {@code SimpleTimeZone}, whose {@code ID} is by
 * contract a label and whose offset is the {@code rawOffset} its constructor
 * stored: {@code new SimpleTimeZone(0, "America/Sao_Paulo")} is UTC that
 * happens to be NAMED {@code America/Sao_Paulo}.
 *
 * WHY THIS VECTOR IS SHAPED THE WAY IT IS. Two facts make the label defect
 * nearly invisible, and both would make a lazier vector vacuous:
 *
 *   * the overwhelmingly common idiom in real code is
 *     {@code new SimpleTimeZone(0, "Z")}. {@code "Z"} resolves to no zone, so
 *     the wrong code path falls back to 0 and is ACCIDENTALLY RIGHT.
 *   * on a UTC host the defect is entirely invisible, because the resolved
 *     offset and the passed {@code rawOffset} are both zero.
 *
 * So every zone id below is a FIXED literal, never the host default, and the
 * load-bearing rows pair a NON-ZERO {@code rawOffset} with a REAL zone id whose
 * true offset DIFFERS from it. {@code "Z"} and {@code "UTC"} appear only as
 * controls that must not move.
 *
 * MUTATION CHECK, so this cannot rot into a vacuous test: delete the
 * {@code MISMATCHED} block and the remaining assertions all pass on a VM with
 * the label defect; delete {@code dstFamily()} and the remaining assertions all
 * pass on a VM whose {@code inDaylightTime}/{@code useDaylightTime}/
 * {@code getDSTSavings}/six-arg {@code getOffset} answer from a null transition
 * table. Those two blocks are the test.
 *
 * THE DST FAMILY, and why it is HERE rather than in a vector of its own. It is
 * the same surface, it is the half a fix to the other half most easily breaks,
 * and it was measured wrong on 542 of the 632 ids
 * {@code TimeZone.getAvailableIDs()} returns:
 *
 *   * {@code inDaylightTime(Date)} is the SILENT one, twice over. On a real
 *     database zone it read a null {@code transitions} array and answered
 *     {@code false} at every instant. On a {@code SimpleTimeZone} its real
 *     bytecode is {@code getOffset(d.getTime()) != this.rawOffset} -- it reads
 *     the FIELD directly while calling the (hijacked) {@code getOffset}, so a
 *     zone constructed with NO daylight-saving rule at all reported that it IS
 *     in daylight time while {@code useDaylightTime()} and
 *     {@code getDSTSavings()} kept saying there was no rule. Both shapes are
 *     asserted.
 *   * {@code getDSTSavings()} must be the RECURRING saving, not any saving in
 *     the zone's history: {@code America/Sao_Paulo} has a dense DST past and
 *     answers 0, because Brazil abolished DST in 2019.
 *   * {@code Australia/Lord_Howe} saves THIRTY minutes and
 *     {@code Pacific/Chatham} sits at {@code +12:45}. A rule that hard-codes an
 *     hour, or that reads a whole-hour offset field, passes every other row
 *     here and fails those two.
 *   * the six-arg {@code getOffset(era,y,m,d,dow,ms)} takes STANDARD local
 *     time, so it must resolve the instant through the standard offset and
 *     then report the total. Its argument contract (message-less
 *     {@code IllegalArgumentException} out of range, {@code dayOfWeek} ignored)
 *     is asserted with it.
 *   * one millisecond either side of a real transition, in both directions, in
 *     both hemispheres.
 *
 * {@code TimeZone.getTimeZone(id)} must keep answering from tzdb. Any fix that
 * removes the SimpleTimeZone natives must not take these with it. HotSpot
 * returns {@code sun.util.calendar.ZoneInfo} here and never a SimpleTimeZone.
 *
 * Determinism: no host zone, no wall clock, no host locale. Every instant is a
 * fixed epoch-milli literal. {@code observesDaylightTime()} is asked only of
 * zones for which it cannot depend on when the suite runs -- either the zone
 * has a recurring rule (so the answer is {@code true} at every instant) or it
 * has neither a rule nor any standing daylight offset (so it is {@code false}
 * at every instant). Zones like {@code Africa/Casablanca}, whose answer is
 * genuinely a function of today's date, are deliberately absent.
 *
 * OUTPUT CONTRACT. Every assertion publishes the VM's OWN answer on its own
 * {@code CK} line before it is compared, so the cross-VM diff is a comparison
 * of two sets of measurements and not of two copies of this fixture's
 * expectations. A failing row is therefore VISIBLE in the diff rather than only
 * inferable from a missing banner. The run ends with:
 * <pre>
 *   CK RSimpleTimeZoneRaw checks=N
 *   CK RSimpleTimeZoneRaw fails=0
 *   PASS RSimpleTimeZoneRaw (N checks)
 * </pre>
 * and on a red run with the same two CK lines, a non-zero {@code fails}, no
 * banner and a non-zero exit. Nothing is printed on any other prefix.
 */
public class RSimpleTimeZoneRaw {
    static int checks;
    static int fails;
    static String firstFailure;

    /** Publishes the observed value, then compares it. */
    static void ck(String key, String actual, String expected) {
        System.out.println("CK RSimpleTimeZoneRaw " + key + "=" + actual);
        checks++;
        if (!actual.equals(expected)) {
            fails++;
            if (firstFailure == null) {
                firstFailure = key + ": expected " + expected + " but was " + actual;
            }
        }
    }

    static void eq(String key, int actual, int expected) {
        ck(key, Integer.toString(actual), Integer.toString(expected));
    }

    static void eq(String key, long actual, long expected) {
        ck(key, Long.toString(actual), Long.toString(expected));
    }

    static void is(String key, boolean actual, boolean expected) {
        ck(key, Boolean.toString(actual), Boolean.toString(expected));
    }

    /** 2021-01-15T12:00:00Z -- northern winter, southern summer. */
    static final long JAN = 1610712000000L;
    /** 2021-07-15T12:00:00Z -- northern summer, southern winter. */
    static final long JUL = 1626350400000L;
    /** 1850-01-15T00:00:00Z -- before the legacy ZoneInfo 1900 floor. */
    static final long OLD = -3786825600000L;
    /** 2045-03-15T00:00:00Z -- past the end of every stored transition table. */
    static final long FAR = 2373062400000L;

    /** UTC-03:00, and with no daylight saving at all since 2019. */
    static final String SAO_PAULO = "America/Sao_Paulo";
    /** UTC-05:00 standard, UTC-04:00 in daylight time. */
    static final String NEW_YORK = "America/New_York";

    public static void main(String[] args) {
        opaqueLabel();
        mismatched();
        dstRuleOnAMismatchedLabel();
        sixArgDiscriminator();
        mutation();
        realZonesStillResolve();
        dstFamily();
        transitionEdges();
        sixArgContract();
        System.out.println("CK RSimpleTimeZoneRaw checks=" + checks);
        System.out.println("CK RSimpleTimeZoneRaw fails=" + fails);
        if (fails != 0) {
            // A red run has already published every observable and both counts,
            // so the harness can see WHAT diverged. Fail loudly after that, not
            // instead of it.
            throw new AssertionError(fails + " check(s) failed; first: " + firstFailure);
        }
        // The banner run.sh actually looks for. Its grep and guard G4 both use
        // `^PASS <Class>`, and harness_check_count parses
        // `PASS <Class> (N checks)` or `CK <Class> checks=N` and NOTHING else.
        System.out.println("PASS RSimpleTimeZoneRaw (" + checks + " checks)");
    }

    /** The id contributes nothing, for every id, resolvable or not. */
    static void opaqueLabel() {
        for (String id : new String[] { SAO_PAULO, NEW_YORK, "UTC", "Z", "GMT+05:00" }) {
            SimpleTimeZone z = new SimpleTimeZone(0, id);
            eq("stz.raw[0," + id + "]", z.getRawOffset(), 0);
            eq("stz.offJan[0," + id + "]", z.getOffset(JAN), 0);
            eq("stz.offJul[0," + id + "]", z.getOffset(JUL), 0);
            ck("stz.id[0," + id + "]", z.getID(), id);
            is("stz.use[0," + id + "]", z.useDaylightTime(), false);
            eq("stz.dst[0," + id + "]", z.getDSTSavings(), 0);
            is("stz.inJan[0," + id + "]", z.inDaylightTime(new Date(JAN)), false);
            is("stz.inJul[0," + id + "]", z.inDaylightTime(new Date(JUL)), false);
        }
    }

    /**
     * THE BLOCK THAT IS THE TEST for the label half. A non-zero rawOffset
     * paired with a real zone id whose true offset differs from it.
     */
    static void mismatched() {
        int[] raws = { 18000000, -18000000, 3600000 };
        String[] ids = { SAO_PAULO, NEW_YORK };
        for (int raw : raws) {
            for (String id : ids) {
                SimpleTimeZone z = new SimpleTimeZone(raw, id);
                String t = "[" + raw + "," + id + "]";
                eq("mm.raw" + t, z.getRawOffset(), raw);
                eq("mm.offJan" + t, z.getOffset(JAN), raw);
                eq("mm.offJul" + t, z.getOffset(JUL), raw);
                // The silent triple. A zone with NO rule cannot be in daylight
                // time at any instant, and inDaylightTime must agree with the
                // two field-backed accessors that report the absence of a rule.
                is("mm.use" + t, z.useDaylightTime(), false);
                is("mm.obs" + t, z.observesDaylightTime(), false);
                eq("mm.dst" + t, z.getDSTSavings(), 0);
                is("mm.inJan" + t, z.inDaylightTime(new Date(JAN)), false);
                is("mm.inJul" + t, z.inDaylightTime(new Date(JUL)), false);
            }
        }
    }

    /**
     * A REAL DST rule hung on a MISMATCHED label. The US rule on rawOffset
     * -05:00, labelled "UTC". HotSpot applies the rule and ignores the label.
     */
    static void dstRuleOnAMismatchedLabel() {
        SimpleTimeZone z = new SimpleTimeZone(
                -18000000, "UTC",
                Calendar.MARCH, 8, -Calendar.SUNDAY, 7200000,
                Calendar.NOVEMBER, 1, -Calendar.SUNDAY, 7200000,
                3600000);
        eq("ruled.raw", z.getRawOffset(), -18000000);
        eq("ruled.offJan", z.getOffset(JAN), -18000000);
        eq("ruled.offJul", z.getOffset(JUL), -14400000);
        is("ruled.inJan", z.inDaylightTime(new Date(JAN)), false);
        is("ruled.inJul", z.inDaylightTime(new Date(JUL)), true);
        is("ruled.use", z.useDaylightTime(), true);
        is("ruled.obs", z.observesDaylightTime(), true);
        eq("ruled.dst", z.getDSTSavings(), 3600000);
        eq("ruled.six.jan", z.getOffset(GregorianCalendar.AD, 2021, Calendar.JANUARY, 15,
                Calendar.FRIDAY, 43200000), -18000000);
        eq("ruled.six.jul", z.getOffset(GregorianCalendar.AD, 2021, Calendar.JULY, 15,
                Calendar.THURSDAY, 43200000), -14400000);

        // Half-hour savings on a hand-built rule: the shape a hard-coded hour
        // gets wrong.
        SimpleTimeZone h = new SimpleTimeZone(37800000, "LHI",
                Calendar.OCTOBER, 1, -Calendar.SUNDAY, 7200000,
                Calendar.APRIL, 1, -Calendar.SUNDAY, 7200000,
                1800000);
        eq("half.dst", h.getDSTSavings(), 1800000);
        eq("half.offJan", h.getOffset(JAN), 39600000);
        eq("half.offJul", h.getOffset(JUL), 37800000);
        is("half.inJan", h.inDaylightTime(new Date(JAN)), true);
        is("half.inJul", h.inDaylightTime(new Date(JUL)), false);
        is("half.use", h.useDaylightTime(), true);
    }

    /**
     * The discriminator. {@code getOffset(int,int,int,int,int,int)} on a
     * SimpleTimeZone runs real bytecode off the real {@code rawOffset} field.
     * If this passes while {@link #mismatched()} fails, the constructor stored
     * the argument correctly and the one-arg accessors are what is wrong.
     */
    static void sixArgDiscriminator() {
        SimpleTimeZone z = new SimpleTimeZone(18000000, SAO_PAULO);
        eq("six.discriminator",
                z.getOffset(GregorianCalendar.AD, 2021, Calendar.JULY, 15, Calendar.THURSDAY,
                        43200000),
                18000000);
    }

    /** setRawOffset must be observable, on a zone whose id resolves elsewhere. */
    static void mutation() {
        SimpleTimeZone z = new SimpleTimeZone(0, SAO_PAULO);
        z.setRawOffset(3600000);
        eq("mut.raw", z.getRawOffset(), 3600000);
        eq("mut.off", z.getOffset(JAN), 3600000);
    }

    /**
     * THE CONTROL, and the thing a fix to the label half most easily breaks.
     * Real database zones must keep their real offsets and their real
     * transitions.
     */
    static void realZonesStillResolve() {
        TimeZone sp = TimeZone.getTimeZone(SAO_PAULO);
        ck("real.sp.class", sp.getClass().getName(), "sun.util.calendar.ZoneInfo");
        eq("real.sp.raw", sp.getRawOffset(), -10800000);
        eq("real.sp.offJan", sp.getOffset(JAN), -10800000);
        eq("real.sp.offJul", sp.getOffset(JUL), -10800000);
        is("real.sp.use", sp.useDaylightTime(), false);

        TimeZone ny = TimeZone.getTimeZone(NEW_YORK);
        eq("real.ny.raw", ny.getRawOffset(), -18000000);
        eq("real.ny.offJan", ny.getOffset(JAN), -18000000);
        eq("real.ny.offJul", ny.getOffset(JUL), -14400000);
        is("real.ny.inJul", ny.inDaylightTime(new Date(JUL)), true);
        is("real.ny.inJan", ny.inDaylightTime(new Date(JAN)), false);
        is("real.ny.use", ny.useDaylightTime(), true);

        TimeZone utc = TimeZone.getTimeZone("UTC");
        eq("real.utc.raw", utc.getRawOffset(), 0);
        eq("real.utc.offJul", utc.getOffset(JUL), 0);
    }

    /** A row per zone per accessor, transcribed from HotSpot 25.0.3+9-LTS. */
    static void zoneRow(String id, int raw, int dst, boolean use, boolean obs,
            int offJan, int offJul, boolean inJan, boolean inJul,
            int sixJan, int sixJul) {
        TimeZone z = TimeZone.getTimeZone(id);
        String t = "[" + id + "]";
        ck("dstf.id" + t, z.getID(), id);
        eq("dstf.raw" + t, z.getRawOffset(), raw);
        eq("dstf.dst" + t, z.getDSTSavings(), dst);
        is("dstf.use" + t, z.useDaylightTime(), use);
        is("dstf.obs" + t, z.observesDaylightTime(), obs);
        eq("dstf.offJan" + t, z.getOffset(JAN), offJan);
        eq("dstf.offJul" + t, z.getOffset(JUL), offJul);
        is("dstf.inJan" + t, z.inDaylightTime(new Date(JAN)), inJan);
        is("dstf.inJul" + t, z.inDaylightTime(new Date(JUL)), inJul);
        eq("dstf.sixJan" + t, z.getOffset(GregorianCalendar.AD, 2021, Calendar.JANUARY, 15,
                Calendar.FRIDAY, 43200000), sixJan);
        eq("dstf.sixJul" + t, z.getOffset(GregorianCalendar.AD, 2021, Calendar.JULY, 15,
                Calendar.THURSDAY, 43200000), sixJul);
    }

    static void dstFamily() {
        //        id                     raw        dst      use    obs    offJan     offJul     inJan  inJul  sixJan     sixJul
        zoneRow(NEW_YORK,             -18000000, 3600000, true,  true,  -18000000, -14400000, false, true,  -18000000, -14400000);
        zoneRow("Europe/London",              0, 3600000, true,  true,          0,   3600000, false, true,          0,   3600000);
        // Thirty-minute daylight saving.
        zoneRow("Australia/Lord_Howe", 37800000, 1800000, true,  true,   39600000,  37800000, true,  false,  39600000,  37800000);
        // A quarter-hour standard offset, and an hour of saving on top of it.
        zoneRow("Pacific/Chatham",     45900000, 3600000, true,  true,   49500000,  45900000, true,  false,  49500000,  45900000);
        // Half-hour offset, no daylight saving, ever.
        zoneRow("Asia/Kolkata",        19800000,       0, false, false,  19800000,  19800000, false, false,  19800000,  19800000);
        // A dense DST HISTORY and no current rule: getDSTSavings must be 0.
        zoneRow(SAO_PAULO,            -10800000,       0, false, false, -10800000, -10800000, false, false, -10800000, -10800000);
        zoneRow("UTC",                        0,       0, false, false,          0,         0, false, false,         0,         0);

        // A fixed custom-offset id, which is not in getAvailableIDs() and takes
        // a different resolution path.
        TimeZone g = TimeZone.getTimeZone("GMT+05:30");
        ck("dstf.gmt.id", g.getID(), "GMT+05:30");
        eq("dstf.gmt.raw", g.getRawOffset(), 19800000);
        eq("dstf.gmt.dst", g.getDSTSavings(), 0);
        is("dstf.gmt.use", g.useDaylightTime(), false);
        is("dstf.gmt.obs", g.observesDaylightTime(), false);
        is("dstf.gmt.inJul", g.inDaylightTime(new Date(JUL)), false);

        // An id no catalog knows is GMT, not an exception.
        TimeZone u = TimeZone.getTimeZone("Nowhere/Nozone");
        ck("dstf.unknown.id", u.getID(), "GMT");
        eq("dstf.unknown.raw", u.getRawOffset(), 0);
        is("dstf.unknown.use", u.useDaylightTime(), false);
        is("dstf.unknown.inJul", u.inDaylightTime(new Date(JUL)), false);

        // Before the first stored transition, and past the last stored rule.
        TimeZone ny = TimeZone.getTimeZone(NEW_YORK);
        eq("dstf.ny.offOld", ny.getOffset(OLD), -18000000);
        is("dstf.ny.inOld", ny.inDaylightTime(new Date(OLD)), false);
        eq("dstf.ny.offFar", ny.getOffset(FAR), -14400000);
        is("dstf.ny.inFar", ny.inDaylightTime(new Date(FAR)), true);
        eq("dstf.ny.sixOld", ny.getOffset(GregorianCalendar.AD, 1850, Calendar.JULY, 15,
                Calendar.MONDAY, 43200000), -18000000);

        // inDaylightTime(null) is a message-less NullPointerException.
        String thrown = "<no-throw>";
        try {
            ny.inDaylightTime(null);
        } catch (Throwable t) {
            thrown = t.getClass().getName() + "/" + (t.getMessage() == null ? "<null>"
                    : t.getMessage());
        }
        ck("dstf.ny.inNull", thrown, "java.lang.NullPointerException/<null>");
    }

    /** One millisecond either side of a real transition, in both hemispheres. */
    static void edge(String id, String tag, long t, int before, int after) {
        TimeZone z = TimeZone.getTimeZone(id);
        eq("edge." + tag + ".off@-1", z.getOffset(t - 1), before);
        is("edge." + tag + ".in@-1", z.inDaylightTime(new Date(t - 1)), before != z.getRawOffset());
        eq("edge." + tag + ".off@0", z.getOffset(t), after);
        is("edge." + tag + ".in@0", z.inDaylightTime(new Date(t)), after != z.getRawOffset());
        eq("edge." + tag + ".off@+1", z.getOffset(t + 1), after);
        is("edge." + tag + ".in@+1", z.inDaylightTime(new Date(t + 1)), after != z.getRawOffset());
    }

    static void transitionEdges() {
        // 2021-03-14T07:00:00Z EST->EDT, 2021-11-07T06:00:00Z EDT->EST.
        edge(NEW_YORK, "ny.spring", 1615705200000L, -18000000, -14400000);
        edge(NEW_YORK, "ny.fall", 1636264800000L, -14400000, -18000000);
        // 2021-03-28T01:00:00Z GMT->BST, 2021-10-31T01:00:00Z BST->GMT.
        edge("Europe/London", "ldn.spring", 1616893200000L, 0, 3600000);
        edge("Europe/London", "ldn.fall", 1635642000000L, 3600000, 0);
        // Southern hemisphere, half-hour step: 2021-04-04T02:00Z DST->standard,
        // 2021-10-02T15:30Z standard->DST.
        edge("Australia/Lord_Howe", "lhi.fall", 1617462000000L, 39600000, 37800000);
        edge("Australia/Lord_Howe", "lhi.spring", 1633188600000L, 37800000, 39600000);
        // Chatham, +12:45 / +13:45.
        edge("Pacific/Chatham", "cha.fall", 1617458400000L, 49500000, 45900000);
        edge("Pacific/Chatham", "cha.spring", 1632578400000L, 45900000, 49500000);
    }

    /** The six-arg overload's argument contract. */
    static void six(String key, TimeZone z, int era, int year, int month, int day, int dow,
            int millis, String expected) {
        String actual;
        try {
            actual = Integer.toString(z.getOffset(era, year, month, day, dow, millis));
        } catch (Throwable t) {
            actual = t.getClass().getName() + "/" + (t.getMessage() == null ? "<null>"
                    : t.getMessage());
        }
        ck(key, actual, expected);
    }

    static void sixArgContract() {
        TimeZone ny = TimeZone.getTimeZone(NEW_YORK);
        six("six.msNeg", ny, GregorianCalendar.AD, 2021, Calendar.JULY, 15, Calendar.THURSDAY,
                -1, "java.lang.IllegalArgumentException/<null>");
        six("six.msMax", ny, GregorianCalendar.AD, 2021, Calendar.JULY, 15, Calendar.THURSDAY,
                86400000, "java.lang.IllegalArgumentException/<null>");
        six("six.msLast", ny, GregorianCalendar.AD, 2021, Calendar.JULY, 15, Calendar.THURSDAY,
                86399999, "-14400000");
        six("six.eraBad", ny, 7, 2021, Calendar.JULY, 15, Calendar.THURSDAY,
                43200000, "java.lang.IllegalArgumentException/<null>");
        six("six.eraBC", ny, GregorianCalendar.BC, 100, Calendar.JULY, 15, Calendar.THURSDAY,
                43200000, "-18000000");
        // dayOfWeek is accepted and ignored: MONDAY for a Thursday changes
        // nothing.
        six("six.dowIgnored", ny, GregorianCalendar.AD, 2021, Calendar.JULY, 15, Calendar.MONDAY,
                43200000, "-14400000");
        // Inside the spring-forward gap, expressed in standard local time.
        six("six.gap", ny, GregorianCalendar.AD, 2021, Calendar.MARCH, 14, Calendar.SUNDAY,
                7200000, "-14400000");
        six("six.fall", ny, GregorianCalendar.AD, 2021, Calendar.NOVEMBER, 7, Calendar.SUNDAY,
                3600000, "-18000000");
        TimeZone lhi = TimeZone.getTimeZone("Australia/Lord_Howe");
        six("six.lhi.oct", lhi, GregorianCalendar.AD, 2021, Calendar.OCTOBER, 15, Calendar.FRIDAY,
                43200000, "39600000");
        six("six.lhi.apr", lhi, GregorianCalendar.AD, 2021, Calendar.APRIL, 15, Calendar.THURSDAY,
                43200000, "37800000");
    }
}
```

`RSimpleTimeZoneRaw` is **not** in `regression-suite/harness-uncounted.txt` and
must not be added: it publishes a count on both paths after this change, so
adding a row would trip G3's other arm.

## 7. NOMINATIONS

**NOM-1 — `regression-suite/src/RSimpleTimeZoneRaw.java`.** The full
replacement in §6. Measured on both VMs: 271 checks, HotSpot green, CratonVM
42 red rows named in the diff. This is the nomination that makes the vector
able to fail for its own reason; the Rust fix alone does not.

**NOM-2 — `native-builtins/src/lib.rs`, `alloc_synth_timezone` (~`:20270`).**
The fabricated `ZoneInfo`'s `rawOffset` FIELD is seeded from the retired 31-arm
hand table and is `0` for 486 of 632 ids while `getRawOffset()` answers
correctly. REPLACE:

```rust
        let raw_offset_ms = tz_standard_offset_seconds(id_str)
            .unwrap_or(0)
            .saturating_mul(1000);
        ctx.set_field_by_name(obj, "rawOffset", Value::Int(raw_offset_ms));
        ctx.set_field_by_name(obj, "rawOffsetDiff", Value::Int(0));
        ctx.set_field_by_name(obj, "dstSavings", Value::Int(0));
```

WITH:

```rust
        // G17-1: seed the FIELD from the same tzdb this VM's own accessors
        // read. `tz_standard_offset_seconds` is a 31-arm hand table the
        // TZDB-OFFSET note below already describes as superseded; it was still
        // the sole writer of this field, so `rawOffset` read 0 for 486 of the
        // 632 available ids while `getRawOffset()` — the native — answered
        // correctly. Any JDK bytecode reading the field rather than calling the
        // accessor got the stale answer; that is what made
        // `getOffset(era, y, m, d, dow, ms)` return 0 for `Australia/Lord_Howe`
        // and `Pacific/Chatham`. Falls back to the hand table only if tzdb has
        // no entry, so no id that worked before can regress.
        let raw_offset_ms = crate::tzdb::raw_offset_seconds(ctx, id_str)
            .or_else(|| tz_standard_offset_seconds(id_str))
            .unwrap_or(0)
            .saturating_mul(1000);
        ctx.set_field_by_name(obj, "rawOffset", Value::Int(raw_offset_ms));
        ctx.set_field_by_name(obj, "rawOffsetDiff", Value::Int(0));
        // The RECURRING saving, matching `getDSTSavings()` — 0 for a zone that
        // has abolished DST however much of it is in the transition table.
        let dst_savings_ms = crate::tzdb::dst_savings_ms(ctx, id_str).unwrap_or(0);
        ctx.set_field_by_name(obj, "dstSavings", Value::Int(dst_savings_ms));
```

Do **not** treat this as a substitute for §4: the field alone cannot carry
`inDaylightTime`, because `ZoneInfo`'s bytecode gates on `transitions` and that
array stays null.

**NOM-3 — `native-builtins/src/lib.rs`, next to
`register_tzdb_offset_natives_for(registry, "sun/util/calendar/ZoneInfo")`
(`:20671`).** Move the `crate::tzdb::register_zoneinfo_dst_natives(registry)`
call there from `date_format_fast::register_date_format_fast`, and delete it
from that file. Two constraints must hold at the new site and both do: it is
reached in the real-JDK arm, and it must not be inside the
`#[cfg(feature = "synthetic-jdk")]` module. Register on `ZoneInfo` only — §4
gives the reason the base is excluded, and moving the call must not quietly
widen it.

**NOM-4 — `native-builtins/src/lib.rs`, the comment on `alloc_synth_timezone`.**
Its "`transitions` stays null, so `ZoneInfo.getOffset(long)` returns this
rawOffset for all instants" is now false of `getOffset(long)` (registered) and
was the load-bearing sentence for four other methods that nobody re-read. It
should say which members the null array still governs and which are served by
natives — `HANDOFF-20260814` §5's "comments actively lie about it".

**NOM-5 — `regression-suite/run.sh`.** `RSimpleTimeZoneRaw` is scheduled and
running (it produced the measurements above), so C12-1's scheduling nomination
has landed. Nothing further. Recorded so the next lane does not re-derive it.

## 8. What this lane did NOT do

* **It did not build or run the changed Rust.** The orchestrator holds the
  release build and this lane was instructed not to touch `cargo`. The §2
  "before" column and the §6 fixture measurements are real; the Rust "after" is
  not. §5 is the strongest substitute available — the *rule* validated on 9,480
  oracle rows — but it is not the VM executing. **The first thing the next lane
  should do is rebuild and re-run `RSimpleTimeZoneRaw`, `RSimpleDateFormatZone`,
  `RJdkFormatLocale` and `RFileTimes`**, all four of which were measured green
  (or, for the first, red at exactly the assertion above) on the pre-fix binary
  in this session.
* **It did not register anything on `java/util/TimeZone`.** §4 gives the
  reason. The consequence is that a receiver with no bytecode of its own — the
  `--synthetic-jdk` stub, and `alloc_synth_timezone`'s
  `ZoneInfo`-less fallback — still answers `false`/`0` for the DST family. That
  population was `invocations=0` in every run measured here and this lane could
  not exercise it.
* **It did not touch `lang_string.rs` (`%t`) or `phases_late/text_intl.rs`.**
  `%tZ`/`%tz` read the same zone surface and F22-1/F28-1 own it. Whether the
  `%t` engine reaches `TimeZone.inDaylightTime` or computes its own offset was
  not established.
* **It did not touch `date_format_fast`'s zone arm.** C12-1 §5's nomination
  has already landed — `simple_tz_class` is gone from `Slots` and
  `vm_implemented` is `zoneinfo_class || timezone_class`. Verified by reading,
  not re-measured.
* **It did not settle `observesDaylightTime` on a zone that is currently in
  daylight time and is abolishing it.** `useDaylightTime() || inDaylightTime(now)`
  and `ZoneInfo`'s "any FUTURE transition carries DST" agree on all 632 ids
  today. A zone in that state would separate them, and none exists in this
  catalog to measure against.
* **It did not re-examine `getOffsets(J[I)I` / `getOffsetsByWall(J[I)I`.** Both
  read `invocations=0` here, yet `GregorianCalendar`'s `DST_OFFSET` /
  `ZONE_OFFSET` were measured CORRECT on CratonVM across a DST boundary
  (`gc.ny.jul.dstOffset=3600000`). Which body supplies that is unexplained and
  is the one loose end in §1.
* **It did not regenerate the stale baselines** C12-1 §6 step 4 names
  (`scripts/baselines/jdk-only-kind-map-25-linux.tsv:7255-7258`,
  `jdk-only-dead-everywhere.tsv:163`). Five new `ZoneInfo` rows now exist that
  those files also do not describe.

## 9. Residuals

1. **`getDSTSavings()` is derived from `lastRules` alone.** That is measured
   right on 632 ids, but it is a *derivation* of a field
   `sun.util.calendar.ZoneInfoFile` computes by its own route. A zone whose
   `lastRules` and whose `simpleTimeZoneParams` disagree would separate them.
   None does today.
2. **`observesDaylightTime()` reads the wall clock**, so it is the one member
   of this family whose answer is not a pure function of (zone, instant). The
   corrected fixture asks it only of zones where the answer cannot move — see
   its class comment. `Africa/Casablanca` is deliberately absent, and a fixture
   that adds it will flake around Ramadan.
3. **`sun/util/calendar/ZoneInfo` keeps C12-1 residual 1's exposure**: a
   `ZoneInfo` built through its public `(String, int)` constructor with an
   `--add-exports` would have a `rawOffset` contradicting its id, and every
   native in §4 would answer from the id. Not reachable from ordinary code.
4. **The `%t` formatter and `SimpleDateFormat`'s `z`/`zzzz` arms consume this
   surface** and were not measured against it. `RSimpleDateFormatZone` is green
   (115 checks, differential-clean) on the pre-fix binary, which means its
   expectations are satisfied by a VM in which no zone observes daylight
   saving. That is worth a second look after the build: a fixture that is green
   both before and after a change this size is either well-scoped or blind, and
   `D3-1` already records that its predecessor was the latter.
