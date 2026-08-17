# G28-1 — the DST rule layer rebuilt, and what re-measuring G17-1 confirmed

> **RECONCILED 2026-08-17 (lane G40) — the rule layer is CONFIRMED GREEN; one
> restated inference is weakened.**
>
> * **Confirmed.** `RSimpleTimeZoneRaw` passes, **MEASURED** on the `9964ca733`
>   binary under `--jdk-only`. The prediction of 74 divergences → 0, made from a
>   rule validated at 632 zones × 9,480 rows against `java.time.zone.ZoneRules`
>   by a lane that could not build, **held exactly**. That is one of the few
>   places in this directory where a PREDICTED value was checked and survived.
> * **Weakened.** §1 restates `G17-1`'s finding that *"`java/util/TimeZone`'s
>   offset family reads `invocations=0`"* and uses it to justify narrow
>   registration. `G33-1` established that `invocations` is a **floor** — it
>   counts only registry-resolved dispatches, so `invocations == 0` proves
>   nothing. The observation that `getID` and `getTimeZone` on the same class run
>   heavily is a real control against "the class is cold"; it is not a control
>   against the intrinsic cache or a JIT direct-call helper. See `INDEX.md` §B.1.

**Status:** BEFORE **RE-MEASURED** on both VMs this session, on the committed
`RSimpleTimeZoneRaw` and on the whole 632-id catalog. The RULE the fix
implements is **MEASURED** against the oracle on 9,480 rows / 632 zones, 0
mismatches. The Rust "after" is **UNMEASURED — needs a build**; this lane was
instructed not to run `cargo`, exactly as G17-1's lane was.
**Provenance:** MEAS on both VMs unless a row says otherwise. Oracle is HotSpot
25.0.3+9-LTS at `$JAVA_HOME`. CratonVM binary is
`C:/craton/target-fcheck/release/cratonvm.exe` (built 2026-08-17 01:41), run
`--jdk-only`. Probes: `scratchpad/g28/{G28Model,G28All,G28Values}.java`.

**Source:** `G17-1-the-dst-family-and-the-fixture-that-compared-two-empty-strings-20260817.md`.
That record's Rust half was destroyed when the lane that wrote it edited the
wrong worktree; the record itself survived and is the specification this lane
rebuilt from. Its VECTOR half landed independently and is what this lane
measured against. This record says plainly, in §1, which of G17-1's claims were
re-verified here and which were carried over on its word.

---

## 0. The headline

| thing | before (RE-MEASURED here) | after |
|---|---|---|
| `RSimpleTimeZoneRaw` on CratonVM | **393 checks, 74 fails, 319 matching** | rule closes all 74; VM re-run pending a build |
| the 74, by member | 18 `getDSTSavings` · 18 `useDaylightTime` · 18 `observesDaylightTime` · 19 `inDaylightTime` · 1 six-arg `getOffset` | five natives, one per member |
| whole-catalog census, 632 ids × 11 accessors | **542 of 632 ids wrong** | rule validated 9,480/9,480 against the oracle |
| `getRawOffset` / `getOffset(long)` | correct on all 632 | untouched, and the fix must keep them so |

Every one of the 74 divergences is in the daylight-saving family. Not one is in
the offset family. That is the same split G17-1 measured, on a differently
shaped fixture, four days later.

---

## 1. What was re-verified, and what was carried over

**RE-VERIFIED here, by running it:**

* **The census: 542 of 632.** `G28All` asks eleven accessors of every id
  `TimeZone.getAvailableIDs()` returns, on both VMs. The per-field breakdown
  reproduces G17-1's to the digit:

  ```
  zones=632  zones differing=542
    s6Jul 532  s6Jan 493  obs 217  dst 214  use 214  iJul 197  iJan 37
    id 0  raw 0  oJan 0  oJul 0
  ```

* **The registry facts (G17-1 §1).** `--dump-native-registry` before the main
  class, Windows path, on this binary:

  ```
  sun/util/calendar/ZoneInfo getOffset        (J)I   owns_slot=true inv=1264 by=lib.rs:20594 overwrote=null
  sun/util/calendar/ZoneInfo getRawOffset     ()I    owns_slot=true inv=632  by=lib.rs:20657 overwrote=null
  sun/util/calendar/ZoneInfo getOffsets       (J[I)I owns_slot=true inv=0    by=lib.rs:20606 overwrote=null
  sun/util/calendar/ZoneInfo getOffsetsByWall (J[I)I owns_slot=true inv=0    by=lib.rs:20623 overwrote=null
  java/util/TimeZone         getOffset        (J)I   owns_slot=true inv=0    by=lib.rs:20594 overwrote=null
  java/util/TimeZone         getRawOffset     ()I    owns_slot=true inv=0    by=lib.rs:20657 overwrote=null
  ```

  and, searched across all **10,651** registered triples:

  * `getDSTSavings`, `useDaylightTime`, `observesDaylightTime`,
    `inDaylightTime` and `getOffset(IIIIII)I` appear in **NO row, on any
    class**. Confirmed.
  * `java/util/TimeZone`'s offset family reads `invocations=0`. Confirmed —
    while `getID` (631) and `getTimeZone` (632) on the same class run heavily,
    so this is a fact about that family and not about the class being cold.
  * `native-builtins/src/date_format_fast.rs:953` **owns a slot** under
    `--jdk-only` (`java/text/DateFormat.format(Ljava/util/Date;)…`), and
    `util_time.rs` appears in **zero** rows of the dump. Both confirmed. That
    is the whole argument for the call site in §3.

* **The exception spellings (G17-1's "transcribed, not derived" pair).**
  Measured on HotSpot: `ZoneInfo.inDaylightTime(null)` is
  `java.lang.NullPointerException` with `getMessage() == null`; all four
  six-arg argument rejections are `java.lang.IllegalArgumentException` with
  `getMessage() == null`. And read out of this VM's own
  `types/src/error.rs:1554`: an `IllegalArgumentException` whose `message` is
  the EMPTY string is converted to a Java exception with a **null** message,
  while `Some("")` would build a non-null empty one. So
  `RuntimeError::IllegalArgumentException { message: String::new() }` and
  `RuntimeError::NullPointerException { message: None }` are the two spellings
  that reproduce HotSpot, exactly as G17-1 said.

* **The three zones that separate `use` from `obs`.** `Africa/Casablanca`,
  `Africa/El_Aaiun`, `Africa/Windhoek`: `useDaylightTime()=false`,
  `observesDaylightTime()=true`, `getDSTSavings()=0`, and in daylight time at
  BOTH probe instants. Measured on the oracle.

* **The rule, on 9,480 oracle rows.** See §2.

**CARRIED OVER on G17-1's word, not re-measured here:**

* That `alloc_synth_timezone`'s `ZoneInfo` leaves `transitions` null. Read in
  `lib.rs`, not observed from Java.
* That a base-class registration would recreate the `C6-2` defect for an
  application's own `extends TimeZone`. The reasoning is sound and `C6-2` is a
  measured record, but no such subclass was exercised here — the same gap
  G17-1 §8 records.
* G17-1's fixture measurements (271 checks / 42 red rows). That fixture was
  superseded before it landed; the committed vector reads 393/74 and is what
  this lane measured.

**CORRECTED.** G17-1 §3 says the six-arg is wrong because the `rawOffset`
FIELD is seeded from a 31-arm hand table. The field half of that nomination has
since LANDED in `lib.rs` (`alloc_synth_timezone` now seeds `rawOffset` from
`crate::tzdb::raw_offset_seconds`) — but the binary measured here predates it,
which is why `s6Jan` still reads 493 above and CratonVM still answers literally
`0` for 486 of them. The `dstSavings` half has NOT landed and cannot until this
record's `dst_savings_ms` exists; see NOM-1.

## 2. The rule is MEASURED, on 632 zones, without a build

`scratchpad/g28/G28Model.java` computes every quantity the Rust computes —
`dstSavings`, `useDaylightTime`, `observesDaylightTime`, `inDaylightTime`, the
legacy total/saving split and the six-arg `getOffset` — **only from
`java.time.zone.ZoneRules`**: the same `standardTransitions` /
`savingsInstantTransitions` / `wallOffsets` / `lastRules` that `tzdb.rs` parses
out of `tzdb.dat`. It compares each against `java.util.TimeZone`'s own answer,
on HotSpot, for every available id at fifteen fixed instants and arguments:

```
IDS=632  ZONES=632  SKIPPED=0  ROWS=9480  MISMATCHES=0
```

That is not the Rust running. It is the *rule* the Rust implements, checked
against the oracle, so the remaining risk is a transcription risk and not a
modelling one. It is also the same number G17-1 reported, arrived at
independently.

**The rules are transcriptions, not inventions.** Each was read out of
`$JAVA_HOME/lib/src.zip` — `sun/util/calendar/ZoneInfoFile.java` (which builds
the `ZoneInfo` from these arrays) and `sun/util/calendar/ZoneInfo.java` (which
answers from it):

| member | the rule | the JDK line it comes from |
|---|---|---|
| `getDSTSavings` | saving of the START rule of the last pair, the pair SWAPPED when the first steps back and the second steps forward | `ZoneInfoFile:573` |
| `useDaylightTime` | `simpleTimeZoneParams != null` — written by the same two branches that write `dstSavings`, so "`dstSavings != 0`" is that test | `ZoneInfo:449` |
| `inDaylightTime(d)` | offset at the instant ≠ standard offset at the instant, and `false` outright below the 1900 floor | `ZoneInfo:485`, `ZoneInfoFile.addTrans` |
| `observesDaylightTime` | `useDaylightTime()`, else scan the table from NOW forward for any entry carrying a saving | `ZoneInfo:453` |
| six-arg `getOffset` | subtract the zone's FIXED raw offset from the standard-local reading, then read the total offset at the resulting instant | `ZoneInfo:367` |

Three of those are worth stating as the traps they are:

* **`getDSTSavings` is a property of `lastRules`, never of the transition
  history.** `America/Sao_Paulo` has a dense DST past and answers `0`, because
  Brazil abolished it in 2019. `America/Mexico_City` and `Asia/Tehran` answer
  `0` while still being IN daylight time at the July 2021 probe, because their
  explicit tables carried a transition their rules no longer do.
* **The swap is load-bearing.** Without it every southern-hemisphere zone
  answers a NEGATIVE saving: `Pacific/Auckland`, `America/Santiago`,
  `Australia/Lord_Howe`.
* **`observesDaylightTime` is not `useDaylightTime`.** The three
  Casablanca/El_Aaiun/Windhoek ids are the whole reason the scan exists.

**One branch is transcribed and UNEXERCISED**, and the record says so rather
than letting the next reader assume it was measured: `ZoneInfoFile`'s
"Israel/Iran workaround" (`ZoneInfoFile:600-657`), which synthesises a saving
for a zone that has no recurring rule but whose explicit table runs to
`LASTYEAR` (2100). Deleting that branch from `G28Model` leaves the 9,480-row
comparison at **0 mismatches** — no zone in this JDK's tzdb reaches it, because
every zone whose table reaches 2100 also has `lastRules`. It is implemented
anyway because it is `ZoneInfoFile`'s own arithmetic and a later tzdb can reach
it; it is not a guess, and it is not measured.

## 3. What this lane changed

Two files, both this lane's.

**`native-builtins/src/tzdb.rs`** — the rule layer and the five natives.

Rule level (pure, `NativeContext`-free, unit-tested against the real
`tzdb.dat` the test module already loads):

* `dst_savings_seconds(rules)`
* `uses_daylight_time(rules)`
* `in_daylight_time_of(rules, ms)`
* `observes_daylight_time_of(rules, now_ms)`
* `offset_ms_at_local_standard_of(rules, local_standard_ms)`
* `legacy_offsets_ms_of(rules, ms)` — the existing legacy-floor rule, split out
  of the `ctx`-taking `legacy_offsets_ms` so it is testable. **Behaviour
  unchanged**, including the unknown-id arm, which still answers `(0, 0)`: the
  old body's two `unwrap_or(0)`s reduced to exactly that and the new body says
  it once, out loud.
* `last_savings_transition_year(rules)` (private), `gregorian_date_is_valid`
  (private), and the two `ZoneInfoFile` constants `UTC2100` / `LASTYEAR`.
* `dst_savings_ms(ctx, id)` — the ctx wrapper that unblocks NOM-1.

`register_zoneinfo_dst_natives(registry)` registers exactly five triples, on
`sun/util/calendar/ZoneInfo` **and on nothing else**:

```
sun/util/calendar/ZoneInfo getDSTSavings        ()I
sun/util/calendar/ZoneInfo useDaylightTime      ()Z
sun/util/calendar/ZoneInfo observesDaylightTime ()Z
sun/util/calendar/ZoneInfo inDaylightTime       (Ljava/util/Date;)Z
sun/util/calendar/ZoneInfo getOffset            (IIIIII)I
```

**Not on `java/util/TimeZone`, deliberately**, though the sibling offset family
in `lib.rs` is registered on both. `TimeZone.getDSTSavings()` and
`TimeZone.observesDaylightTime()` are **concrete** on the base, so an
application's own `extends TimeZone` that does not override them would have no
bytecode of its own, the superclass climb would run, and it would be answered
from a tzdb lookup of its `ID` — the `C6-2` defect in a new place. The base's
offset rows read `invocations=0` in §1, so the narrow registration loses
nothing measured. **`register_tzdb_offset_natives_for` is called for BOTH
classes, so following the neighbouring call is the wrong instinct here**; the
reason is written on the registrar itself so that a later relocation cannot
quietly widen it.

All five members are DECLARED by `ZoneInfo` itself (`javap -p`, and the source
in `src.zip`), and the existing `ZoneInfo.getOffset(J)I` registration proves
that an exact-class registration wins over a class's own bytecode on this VM
(`owns_slot=true`, `inv=1264`, on a class that declares the method).

`inDaylightTime` reaches the instant through a real
`ctx.invoke_virtual(date, "getTime", "()J")`, pinning both references across
it, because a deprecated `Date` setter can leave `cdate` dirty and force a
normalise — the same reason `phases_late/ssl_security.rs` does it that way for
`checkValidity(Date)`. Slot 0 is the fallback for a synthetic `Date`.

**`native-builtins/src/date_format_fast.rs`** — one call, and the reason it is
there.

`register_date_format_fast` gains
`crate::tzdb::register_zoneinfo_dst_natives(r)`, after `r.set_category(prev)`
so the new triples take the ambient category rather than `Intrinsic`. It is a
strange address and the comment says so. The constraint that produced it:
**`util_time.rs` — the obvious home — is `#[cfg(feature = "synthetic-jdk")]`
and is not compiled into the default build at all**, so a registration there
would be dead code that reads as a fix, the trap `HANDOFF-20260814` §5 records
twice. §1 measures both halves of that: `date_format_fast.rs:953` owns a slot
under `--jdk-only`; `util_time.rs` owns none. Relocation next to
`register_tzdb_offset_natives_for` is NOM-2.

**Unit tests**, in `tzdb.rs`'s existing `#[cfg(test)] mod tests`, all against
the real `tzdb.dat`, every expectation transcribed from the oracle
(`scratchpad/g28/G28Values.java`) and none derived from this module:

* `dst_savings_matches_hotspot_get_dst_savings` — 13 zones, including the
  30-minute (`Australia/Lord_Howe`) and two-hour (`Antarctica/Troll`) savings,
  the three abolished-DST zones, the two standing-offset zones, and the two
  southern-hemisphere zones that the swap is for.
* `uses_daylight_time_matches_hotspot` — 16 zones, including the three
  `use=false obs=true` ones.
* `observes_daylight_time_separates_itself_from_uses` — asserts the separation
  on those three, at a FIXED "now", since this is the one member that reads the
  wall clock.
* `in_daylight_time_matches_hotspot_at_the_fixture_instants` — 14 zones × 4
  instants, including below the 1900 floor and past the end of the stored
  tables, and including `Africa/Cairo`, whose 2021 rule dates put BOTH probes
  in standard time (the row that catches "assume July means daylight").
* `in_daylight_time_is_exact_at_a_transition_boundary` — ±1 ms either side of
  eight real transitions, both hemispheres, both directions.
* `six_arg_offset_reads_local_standard_time` — 10 zones, plus the
  spring-forward gap and the fall-back overlap expressed in standard local
  time, plus both half-hour zones across their own transitions.
* `six_arg_offset_honours_the_1900_floor` — 1850 and era `BC`.
* `gregorian_epoch_day_anchors_and_the_six_arg_date_validation`.
* `last_savings_transition_year_pins_to_lastyear_beyond_utc2100` — the only
  test of the unexercised branch's input, on a synthetic rule set.
* `a_fixed_offset_zone_never_observes_daylight_saving` — the `GMT±HH:MM`
  fallback and, by the same arms, any id the catalog cannot resolve.
* `legacy_offsets_split_total_from_saving` — pins the refactored
  `legacy_offsets_ms_of`'s behaviour, including the half-hour saving.

## 4. What the fix is predicted to do to the 74

All 74 divergences are in the five registered members, and the rule is
oracle-exact on all 632 ids for all five. So the prediction is **74 of 74
closed, `RSimpleTimeZoneRaw` at 393 checks / 0 fails**. The one link that is
NOT measured is whether the registrations are reached at run time; `owns_slot`
for the call site's own triple is the strongest evidence available without a
build, and it is positive.

The rows that must NOT move are the ones the narrow registration is designed to
protect, and they are 319 of the 393 today: every `stz.*` `SimpleTimeZone` row
(the opaque-label half — `SimpleTimeZone` is not a `ZoneInfo`, so it cannot be
captured), every `getRawOffset` and `getOffset(long)` row, `tz.availableIDs.length`,
`tz.New_York.class`, and the unknown-id fallback.

Also measured on the pre-fix binary this session, unchanged: `RSimpleDateFormatZone`
**PASS (115 checks)**, `RJdkFormatLocale` **PASS (20 checks)**, `RFileTimes`
**PASS (68 checks)**.

## 5. NOMINATIONS

**NOM-1 — `native-builtins/src/lib.rs`, `alloc_synth_timezone`, `:20409`.**
This is `G23-1`'s N-TZ-1, now unblocked. The comment at `:20384` states the
blocker exactly: the value "lives in `ZoneRulesData.last_rules` — a PRIVATE
field of `crate::tzdb`, so it cannot be read from here, and `tzdb` exposes no
`dst_savings_ms`". It does now. REPLACE:

```rust
        ctx.set_field_by_name(obj, "dstSavings", Value::Int(0));
```

WITH:

```rust
        // G28-1: the RECURRING saving, from the same producer `getDSTSavings()`
        // now reads. NOT a second producer -- `crate::tzdb::dst_savings_ms` is
        // the same function the native calls, so the field and the accessor
        // cannot drift. This is what the rejected `legacy_offsets_ms` sampler
        // could not be: that one was measured 601/604, wrong on
        // `Africa/Casablanca`, `Africa/El_Aaiun` and `Africa/Windhoek`.
        let dst_savings_ms = crate::tzdb::dst_savings_ms(ctx, id_str).unwrap_or(0);
        ctx.set_field_by_name(obj, "dstSavings", Value::Int(dst_savings_ms));
```

Do **not** treat this as a substitute for the natives: the field alone reaches
`getDSTSavings()` and nothing else, because `useDaylightTime()` reads
`simpleTimeZoneParams` and `observesDaylightTime()`/`inDaylightTime()` read
`transitions`, and both stay null. That is source-verified in `lib.rs`'s own
comment and re-verified here against `src.zip`.

**NOM-2 — `native-builtins/src/lib.rs`, `:20809`.** Move
`crate::tzdb::register_zoneinfo_dst_natives(registry)` next to
`register_tzdb_offset_natives_for(registry, "sun/util/calendar/ZoneInfo")` and
delete it from `date_format_fast.rs:1148`. Two constraints must hold at the new
site and both do: it is reached in the real-JDK arm, and it is not inside a
`#[cfg(feature = "synthetic-jdk")]` module. **Register on `ZoneInfo` only** —
the neighbouring call is made TWICE, once for the base class, and copying that
shape would recreate `C6-2`.

**NOM-3 — `native-builtins/src/lib.rs`, `:20384-20408`.** That comment block is
now stale in both of its halves. It says `dstSavings` stays 0 because `tzdb`
exposes no `dst_savings_ms` (it does now, NOM-1), and it says "The DST family
needs `transitions`/`simpleTimeZoneParams` populated **or a native override**"
— the native override is what this record delivers, and the comment should
point at it rather than leave the next reader concluding the family is still
unreachable. `HANDOFF-20260814` §5's "comments actively lie about it".

**NOM-4 — `regression-suite/src/RSimpleTimeZoneRaw.java`.** The committed
vector asserts the family at two instants and asserts the six-arg on exactly
two rows. It does NOT assert: the transition boundaries (±1 ms, eight of them,
both hemispheres), the six-arg's argument contract (four message-less
`IllegalArgumentException`s, `dayOfWeek` accepted-and-ignored, era `BC`), or
`inDaylightTime(null)`'s message-less NPE. All of those are MEASURED on the
oracle in `scratchpad/g28/G28Values.java` and are exactly the rows a future
regression inside the new natives would hit first — a boundary is where a
guess-and-refine rewrite of `offset_ms_at_local_standard_of` would fail, and
the contract rows are where the `String::new()` / `None` exception spellings
are the only thing standing between HotSpot's null message and a `""`.

**NOM-5 — `scripts/baselines/jdk-only-kind-map-25-linux.tsv:11731-11734`.**
Four `sun/util/calendar/ZoneInfo` rows are described there; five more triples
now exist that the file does not describe. `jdk-only-dead-everywhere.tsv` names
no `ZoneInfo` row at all. Both need regenerating after the build.

## 6. What this lane did NOT do

* **It did not build or run the changed Rust.** The instruction forbade
  `cargo build`/`check`/`test`; the binary at `target-fcheck` is held by the
  orchestrator and predates even the already-landed `rawOffset` field seeding.
  §2 is the strongest substitute available. **The first thing the next lane
  should do is rebuild and re-run `RSimpleTimeZoneRaw`** (74 → 0 predicted),
  then `RSimpleDateFormatZone`, `RJdkFormatLocale` and `RFileTimes`, all three
  measured green here on the pre-fix binary.
* **It did not register anything on `java/util/TimeZone`.** The consequence is
  that a receiver with no bytecode of its own — the `--synthetic-jdk` stub, and
  `alloc_synth_timezone`'s `ZoneInfo`-less fallback — still answers `false`/`0`
  for this family. That population was `invocations=0` in every dump measured
  here and this lane could not exercise it.
* **It did not exercise the `C6-2` argument.** No `extends TimeZone` subclass
  was written or run. The narrow registration is justified by reading and by
  `C6-2`'s own measurements, not by a new one.
* **It did not touch the six-arg's `setRawOffset` exposure.** `ZoneInfo`'s real
  six-arg subtracts the `rawOffset` FIELD; the native subtracts the raw offset
  computed from the `ID`. A `ZoneInfo` whose `setRawOffset` has been called
  would diverge. That is `C12-1` residual 1's exposure, shared with every
  already-registered member of the offset family, and it is not reachable from
  ordinary code.
* **It did not re-examine `getOffsets(J[I)I` / `getOffsetsByWall(J[I)I`.** Both
  read `invocations=0` here, exactly as G17-1 found, and `GregorianCalendar`'s
  `DST_OFFSET`/`ZONE_OFFSET` are correct on CratonVM across a DST boundary.
  Which body supplies that is still unexplained — G17-1's one loose end, still
  loose.
* **It did not touch `lang_string.rs` (`%t`) or `phases_late/text_intl.rs`.**

## 7. Residuals

1. **`getDSTSavings()` is derived from `lastRules`.** Measured right on 632
   ids, but it is a derivation of a field `ZoneInfoFile` computes by its own
   route. A zone whose `lastRules` and whose `simpleTimeZoneParams` disagree
   would separate them; none does today.
2. **`observesDaylightTime()` reads the wall clock**, so it is not a pure
   function of (zone, instant). The unit test pins a fixed "now" for that
   reason, and will need its expectations re-measured if a tzdb update changes
   the status of `Africa/Casablanca`, `Africa/El_Aaiun` or `Africa/Windhoek`.
   The committed fixture asks it only of zones whose answer cannot move.
3. **The Israel/Iran branch of `dst_savings_seconds` is unexercised.** §2.
4. **`RSimpleDateFormatZone` is green both before and after a change this
   size** — 115 checks, on a VM in which no zone observed daylight saving. That
   is either well-scoped or blind, and `D3-1` records that its predecessor was
   the latter. Worth a second look after the build.
