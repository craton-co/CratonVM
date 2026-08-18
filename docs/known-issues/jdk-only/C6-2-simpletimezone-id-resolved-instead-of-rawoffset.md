# C6-2 — `SimpleTimeZone` answers from its ID, and it is four natives, not a constructor

> **SUPERSEDED IN PART, 2026-08-12 by lane C12** —
> `C12-1-simpletimezone-the-trap-and-the-second-site.md`. Both nominations
> LANDED. Three corrections to read before using this record:
> 1. **The trap in 3B is answered: the base registration does NOT capture a real
>    `SimpleTimeZone`.** Not by guessing between the two readings — by the
>    dispatch source, which passes the RECEIVER's class name, skips the
>    superclass climb when the receiver's class declares the method, and keys the
>    late re-check on the declaring class. Real `SimpleTimeZone` declares all
>    three live members (`javap`). C12-1 §2 has the citations; the confirming run
>    is still required and is C12-1 §6 step 1.
> 2. **§D's predicted number is wrong.** Measured, the bc-java parse skew is
>    **7,200,000 ms**, not 10,800,000: São Paulo was observing DST on
>    2002-01-22, and the tzdb path applies the transition for the instant rather
>    than the standard offset. The skew is a time series, not a constant.
> 3. **Residual 3 was the live one.** `date_format_fast.rs:806` reproduces this
>    defect BELOW dispatch, so no unregistration reaches it — and that, not the
>    natives, is where `SimpleDateFormat.format` over an app-built
>    `SimpleTimeZone` actually lands. Measured oracle + nomination in C12-1 §3/§5.

**2026-08-12, lane C6.** The observable was isolated by another lane and is
quoted verbatim in §1. This record does three things that finding did not:
**locates the mechanism**, **shows the defect is not confined to
`getRawOffset()`**, and **names a trap that would make the obvious fix inert**.

**This lane could not build or run the VM.** Every HotSpot number was executed
on this host (HotSpot 25.0.3+9-LTS); every CratonVM value is **PREDICTED** from
the native bodies and marked as such, except the four rows in §1 which another
lane measured.

---

## 1. The observable (measured by another lane, quoted)

> `SimpleTimeZone(int rawOffset, String ID)` DISCARDS the rawOffset argument and
> instead resolves the offset from the ID.
> ```
> SimpleTimeZone(18000000, "America/Buenos_Aires").getRawOffset()
>   HotSpot   = 18000000   (the argument, as the constructor contract requires)
>   CratonVM  = -10800000  (the zone's real offset, i.e. the ID won)
> ```
> Reproduced on BOTH CratonVM modes (`--jdk-only` and `--real-jdk`), so it is
> not a strict-mode artefact.

Two things make it nearly invisible, and both belong in any vector written
against it:

* the overwhelmingly common idiom in real code is `new SimpleTimeZone(0, "Z")`.
  `"Z"` resolves to no zone, so the wrong code path falls back to `0` and is
  **accidentally right**;
* **on a UTC host the defect is entirely invisible**, because the resolved
  offset and the passed `rawOffset` are both zero.

## 2. The mechanism — it is not the constructor

`java.util.SimpleTimeZone`'s constructor is fine. Its `getRawOffset()` is one
instruction:

```
$ javap -p -c --module java.base java.util.SimpleTimeZone
  public int getRawOffset();
    Code:
       0: aload_0
       1: getfield  #24   // Field rawOffset:I
       4: ireturn
```

**That correct implementation is shadowed.** `native-builtins/src/lib.rs:20489`:

```rust
    register_tzdb_offset_natives_for(registry, "java/util/SimpleTimeZone");
```

`register_tzdb_offset_natives_for` registers **four** natives, and every one of
them reads the receiver's `ID` field and resolves it against tzdb:

```rust
        registry.register(class_name, "getRawOffset", "()I", |ctx, args| {
            ...
            let id = match ctx.get_field_by_name(this, "ID") { ... };
            let raw = crate::tzdb::raw_offset_seconds(ctx, &id).unwrap_or(0);
            Ok(Some(Value::Int(raw.saturating_mul(1000))))
        });
```

The receiver's own `rawOffset` field, and its own DST rule, are never consulted.

**Why the registration exists, and why that is the whole bug.** The comment
above it says so plainly: `alloc_synth_timezone` sometimes fabricates a real
`SimpleTimeZone` for `TimeZone.getTimeZone(id)`, and these natives were added so
*that* object would answer from tzdb. **Registration is per CLASS.** It
therefore also captured every `SimpleTimeZone` the *application* constructs —
objects that have real bytecode, real state, and an `ID` that is by contract an
opaque label.

**Confirming the mechanism costs one line of Java, and it is in the vector:**
`getOffset(int,int,int,int,int,int)` is **not** among the four registered
descriptors, so it runs real bytecode off the real `rawOffset` field.

| | HotSpot 25 | CratonVM (PREDICTED) |
|---|---|---|
| `new SimpleTimeZone(18000000, "America/Sao_Paulo").getRawOffset()` | `18000000` | `-10800000` |
| same object, `getOffset(AD, 2021, JULY, 15, THURSDAY, 43200000)` | `18000000` | **`18000000`** |

The six-arg form agreeing while the no-arg form disagrees is the proof that the
constructor stored the argument correctly. **This is a native-registration
defect, not a constructor defect.**

## 3. The defect is not confined to `getRawOffset()` — and one answer is silent

`inDaylightTime(Date)` is not registered. Its real bytecode is:

```
  public boolean inDaylightTime(java.util.Date);
       2: invokevirtual  getOffset:(J)I      <- the HIJACKED native
       9: getfield       #24  // rawOffset:I <- the REAL field
      12: if_icmpeq ...
```

So it compares a **tzdb-resolved offset** against the **real `rawOffset`
field** — two quantities that only agree by accident. The result is a zone
constructed with **no daylight-saving rule at all** reporting that it IS in
daylight time, while `useDaylightTime()` and `getDSTSavings()` — unhijacked and
field-backed — keep correctly reporting that there is no rule.

Demonstrated with a faithful simulation (`scratchpad/c6/Mutant.java`, a
`SimpleTimeZone` subclass overriding only the two hijacked accessors, so
`inDaylightTime` runs the genuine JDK bytecode):

```
RED MISMATCHED inDaylightTime(JUL)(18000000,America/Sao_Paulo) -> true  while useDaylightTime()=false getDSTSavings()=0
RED MISMATCHED inDaylightTime(JUL)(3600000,America/New_York)   -> true  while useDaylightTime()=false getDSTSavings()=0
```

**This is the dangerous half.** `getRawOffset()` being wrong is a wrong number;
this is a self-contradictory object, returned by a boolean method that no
equality-shaped test will ever look at.

A fourth registration is simply **dead**: `SimpleTimeZone` declares no
`getOffsetsByWall(J[I)I` at all (`javap` confirms; it is a `ZoneInfo`-only
method), and `scripts/baselines/jdk-only-dead-everywhere.tsv:163` already
records it as `method-nowhere`. The family was copied from `ZoneInfo` without
checking membership.

## 4. The HotSpot oracle, on fixed zone ids

Full transcript: `scratchpad/c6/C6StzProbe.java`. **No host default zone is read
anywhere**, so these numbers reproduce on any machine. `America/Sao_Paulo` is
UTC-03:00 with no daylight saving since 2019; `America/New_York` is UTC-05:00 /
UTC-04:00. `JAN = 1610712000000L`, `JUL = 1626350400000L`.

**A. two-arg constructor — the ID contributes nothing:**

| construction | HotSpot `getRawOffset()` | `getOffset(JAN)` | `getOffset(JUL)` | `inDaylightTime(JUL)` | `useDaylightTime()` | CratonVM `getRawOffset()` **PREDICTED** |
|---|---|---|---|---|---|---|
| `(0, "America/Sao_Paulo")` | `0` | `0` | `0` | `false` | `false` | `-10800000` |
| `(0, "America/New_York")` | `0` | `0` | `0` | `false` | `false` | `-18000000` |
| `(0, "UTC")` | `0` | `0` | `0` | `false` | `false` | `0` (accident) |
| `(0, "Z")` | `0` | `0` | `0` | `false` | `false` | `0` (accident) |
| `(0, "GMT+05:00")` | `0` | `0` | `0` | `false` | `false` | `18000000` |
| `(18000000, "America/Sao_Paulo")` | `18000000` | `18000000` | `18000000` | `false` | `false` | `-10800000` |
| `(18000000, "America/New_York")` | `18000000` | `18000000` | `18000000` | `false` | `false` | `-18000000` |
| `(-18000000, "America/Sao_Paulo")` | `-18000000` | `-18000000` | `-18000000` | `false` | `false` | `-10800000` |
| `(-18000000, "America/New_York")` | `-18000000` | `-18000000` | `-18000000` | `false` | `false` | `-18000000` (**accident — see below**) |

`getID()` is verbatim on both VMs in every row; that half is correct.

**Do not build a vector on `(-18000000, "America/New_York")`.** The chosen
`rawOffset` happens to equal that zone's real standard offset, so the row passes
on a broken VM. It is retained above only because the corresponding
`getOffset(JUL)` row (`-18000000` vs a predicted `-14400000`) still catches it.
This is the same vacuity trap as `(0, "Z")`, wearing a non-zero number.

**B. eleven-arg constructor — a REAL DST rule on a MISMATCHED label.** US rule
on `rawOffset = -05:00`, labelled `"UTC"`:

| | HotSpot | CratonVM **PREDICTED** |
|---|---|---|
| `getRawOffset()` | `-18000000` | `0` |
| `getOffset(JAN)` | `-18000000` | `0` |
| `getOffset(JUL)` | `-14400000` | `0` |
| `inDaylightTime(JAN)` | `false` | `true` |
| `inDaylightTime(JUL)` | `true` | `true` (accident) |
| `useDaylightTime()` | `true` | `true` (field-backed) |
| `getDSTSavings()` | `3600000` | `3600000` (field-backed) |

**So the "argument discarded in favour of a resolved value" shape DOES repeat in
the longer constructors** — the caller's explicit DST rule is dropped exactly as
the two-arg `rawOffset` is. This answers the question the isolating lane left
open.

**C. `TimeZone.getTimeZone(id)` — the control that must not move:**

```
STZ F America/Sao_Paulo class = sun.util.calendar.ZoneInfo
STZ F America/Sao_Paulo getRawOffset() = -10800000     getOffset(JUL) = -10800000
STZ F America/New_York  class = sun.util.calendar.ZoneInfo
STZ F America/New_York  getRawOffset() = -18000000     getOffset(JAN) = -18000000
STZ F America/New_York  getOffset(JUL) = -14400000     inDaylightTime(JUL) = true
STZ F UTC               class = sun.util.calendar.ZoneInfo   getRawOffset() = 0
```

**HotSpot returns `sun.util.calendar.ZoneInfo` and never a `SimpleTimeZone`.**
That is load-bearing for NOMINATION 3A.

**D. the bc-java idiom that found this**, with a fixed id substituted for the
host default so it is reproducible: `SimpleDateFormat("yyyyMMddHHmmss")` with
`setTimeZone(new SimpleTimeZone(0, "America/Sao_Paulo"))` parsing
`20020122122220` — HotSpot `1011702140000`, CratonVM **PREDICTED**
`1011712940000`, i.e. off by exactly `10800000` ms, matching the five
local-time rows in `P4A-CORPORA-20260812.md` §A.

> **This prediction is WRONG, and `P4A-CORPORA`'s number is not (C18,
> 2026-08-12).** Measured in `C12-1` §4, the `America/Sao_Paulo` gap is
> **7,200,000 ms**, not 10,800,000: São Paulo was observing DST on
> 2002-01-22, and the tzdb path applies the zone's *total* offset at that
> instant rather than its standard offset. `P4A-CORPORA`'s `10800000` is a
> different quantity — the **host default zone** `America/Buenos_Aires`
> (−03:00, no DST then) — and is correct as measured. **The skew is not a
> constant**; it is whatever the zone's total offset was at the instant
> parsed, so a test asserting a fixed delta passes or fails on its choice of
> date.

---

## NOMINATIONS — two parts, and the ORDER matters

**Both halves are required. Landing 3B alone regresses the ~20 zones in
`tz_dst_rule`; landing 3A alone changes nothing.**

### NOMINATION 3A — stop fabricating a `SimpleTimeZone` for a zone id

**File:** `native-builtins/src/lib.rs`, in `alloc_synth_timezone`.

DELETE the whole `tz_dst_rule` branch — the block beginning:

```rust
        if let (Some(std_secs), Some(rule)) =
            (tz_standard_offset_seconds(id_str), tz_dst_rule(id_str))
        {
```

and ending with its closing `}` immediately before the
`// Prefer sun/util/calendar/ZoneInfo` comment, so every fabricated zone takes
the `ZoneInfo` path below it.

**This is not a behaviour trade, it is a deletion of superseded code, and the
file already says so.** The TZDB-OFFSET note at `lib.rs:20334` states the tzdb
work *"Supersedes the previous `tz_dst_rule`/`dst_start_year`/
`historical_lmt_offset` hand-rolled approximations (a ~20-zone allowlist
modeling only each zone's *current* recurring DST rule) — this covers every
zone's full historical transition table instead."* The approximation's
consumers were removed; the branch that produces it was not. And §4C above
shows HotSpot itself returns `ZoneInfo` here, never a `SimpleTimeZone`, so this
moves CratonVM toward the oracle rather than away from it.

### NOMINATION 3B — unregister the family from `SimpleTimeZone`, INCLUDING THE BASE

**File:** `native-builtins/src/lib.rs:20489`.

DELETE:

```rust
    register_tzdb_offset_natives_for(registry, "java/util/SimpleTimeZone");
```

**THE TRAP — this deletion is probably not sufficient on its own, and a lane
that stops here will report a fix that does nothing.** Three lines below it:

```rust
    register_tzdb_offset_natives_for(registry, "java/util/TimeZone");
```

and `java.util.TimeZone.getRawOffset()` is **`abstract`** in the real JDK
(`javap` confirms; likewise `inDaylightTime`). So a `TimeZone`-typed call site —
which is how the API is normally used, and exactly how
`ASN1GeneralizedTime.getDate()` uses it — may resolve to the base declaration
and hit the base registration, with the receiver still a `SimpleTimeZone`. The
existing comment on that line asserts *"A subclass receiver still resolves its
own exact-class registration first"*, which is a claim about receiver-class-first
lookup **with superclass fallback** — and once the subclass registration is
gone, the fallback is precisely what fires.

**Do not adopt either reading from this record.** Establish it, in this order:

1. Build with 3A + the deletion above.
2. Run `RSimpleTimeZoneRaw` (below). If the `MISMATCHED` block is still red,
   the base registration is capturing the receiver.
3. If so, guard the four bodies in `register_tzdb_offset_natives_for` at the
   top: when the receiver's class is `java/util/SimpleTimeZone`, run its own
   bytecode instead of resolving the id —
   `ctx.invoke_virtual_bytecode_only(this, "getRawOffset", "()I", &[])`. That
   helper already exists and is already used a few hundred lines above, at
   `lib.rs:20031`, for `SimpleTimeZone.setStartYear(int)`.

Also drop `getOffsetsByWall` from the `SimpleTimeZone` case regardless: §3 shows
the method does not exist on that class, and the baseline already lists it as
`method-nowhere`.

### The regression vector — LANDED at `regression-suite/src/RSimpleTimeZoneRaw.java`

**Passes on HotSpot 25 today: `CK RSimpleTimeZoneRaw checks=104` / `RESULT ...
PASS`.** Not yet run against any CratonVM binary — that first run is ahead, and
a red is the gate working, not a bad vector.

Honours the isolating lane's explicit requirement: the load-bearing rows pair a
**non-zero `rawOffset`** with a **real zone id whose true offset differs from
it**, in a block named `MISMATCHED`. **Mutation-checked**, not assumed —
`scratchpad/c6/Mutant.java` simulates the four natives and drives that block to
`red=20 / green=4`, with the six-arg discriminator staying green. Deleting the
`MISMATCHED` block makes everything else pass on today's VM; that block is the
test.

**Registration line (this lane may not edit `run.sh`):** in
`regression-suite/run.sh`, append ` RSimpleTimeZoneRaw` to **`CORE_CLASSES`**,
not to `JDKONLY_CLASSES`. The defect reproduces in `--real-jdk` as well, so it
is a default-mode compatibility concern — the same reasoning the file already
records for `RJdkViews`.

REPLACE (end of the `CORE_CLASSES` line):

```
RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics"
```

WITH:

```
RJdkFormatLocale RJdkStrictMath RJdkByteOrder RJdkIntrinsics RSimpleTimeZoneRaw"
```

Without this line the vector compiles and never runs, and `run.sh`'s own list
hygiene will flag it as an unlisted `src/*.java` — that report is the backstop,
not a substitute.

---

## Residuals

1. **The base-class capture in 3B is UNRESOLVED.** This record deliberately
   asserts no rule about it; the two readings of the existing comment predict
   opposite outcomes and the difference is one build away. Measure, do not pick.
2. **`P4A-CORPORA-20260812.md` §B is a separate, undiagnosed divergence** in the
   same area (German locale time pattern and long-vs-short zone display name).
   Nothing here touches it and it should not be folded into this fix.
3. **`date_format_fast.rs:306` caches `simple_tz_class`** and takes a fast path
   keyed on it. It was not examined by this lane. If 3A stops producing
   `SimpleTimeZone` instances from `getTimeZone`, that cache's hit rate and its
   correctness for application-built `SimpleTimeZone`s both need a look.
4. **`getOffsets(J[I)I` on `SimpleTimeZone` is package-private** in the real JDK.
   Whether a registered native on a package-private method is reachable the same
   way was not established; it is registered, and the vector does not exercise
   it directly.
