# C19-2 — `RSimpleDateFormatZone`: the `format`-shaped vector C12-1 §5 asked for, with the vacuity trap closed mechanically

**2026-08-13, lane C19.** C12-1 §5 ends with: *"That fixture does not cover §3.
It exercises the accessors, not `SimpleDateFormat.format`, so it will read green
while the second site is still wrong. A `format`-shaped block using the four
rows of §3 is the missing vector."* This record lands it —
`regression-suite/src/RSimpleDateFormatZone.java`, **109 checks, PASS on HotSpot
25.0.3+9-LTS, mutation-checked in three directions** — and widens the four rows
to ten.

**This lane could not build or run the VM.** Every number below was executed
against HotSpot 25.0.3+9-LTS on this host. Nothing about CratonVM's actual
behaviour is asserted here.

---

## 1. What it targets, and why the two directions are labelled

`date_format_fast.rs:806` computes the zone offset in Rust without dispatching,
from the receiver's `ID` field via tzdb. That is right for a
`sun.util.calendar.ZoneInfo` (whose id IS the zone) and wrong for a
`java.util.SimpleTimeZone` (whose id is by contract an opaque LABEL). It is the
`format` path only: `parse` is not intercepted and reaches the real
`SimpleTimeZone` bytecode.

**The two directions are fixed by different changes** — C12-1 §1 landed the
parse half, C12-1 §5 is the still-unapplied format half — so every block says
which one it targets. A suite that only round-trips reads the half-landed state
as fixed, which is the exact failure mode C12-1 §4 warns about.

| family | checks | direction |
|---|---|---|
| `control` | 8 | neither. NEGATIVE CONTROL: `ZoneInfo` and `GMT+05:30` receivers, which the fast arm should KEEP |
| `fmtdate` | 20 | FORMAT. `DateFormat.format(Date)`, the intercepted descriptor |
| `routes` | 30 | FORMAT. Four dispatch routes must agree with EACH OTHER |
| `roundtrip` | 30 | BOTH, on a zone-less pattern |
| `parse` | 10 | PARSE. The bc-java idiom, plus the measured anchor |
| `dstrule` | 11 | FORMAT, eleven-arg constructor |

## 2. THE VACUITY TRAP, closed mechanically rather than by care

C6-2 warned: *"Do not build a vector on `(-18000000, "America/New_York")`. The
chosen `rawOffset` happens to equal that zone's real standard offset, so the row
passes on a broken VM."* The brief for this lane records that the trap cost an
earlier lane real time.

Care is not a mechanism. Every row-driven block therefore asserts, as its own
first check per row:

```java
static void notVacuous(String row, int raw, String id, long at) {
    int trueOff = TimeZone.getTimeZone(id).getOffset(at);
    check(trueOff != raw, row + ": VACUOUS VECTOR -- rawOffset " + raw + " equals " + id
            + "'s true offset " + trueOff + " at instant " + at + " ...");
}
```

A row that becomes vacuous — because someone edits a number, or because a tzdb
update moves a zone — **fails loudly instead of going quietly green**. §5 shows
that guard firing.

### The ten rows, all measured, all non-vacuous

Fixed instants `JAN = 1610712000000L` (2021-01-15T12:00:00Z) and
`JUL = 1626350400000L` (2021-07-15T12:00:00Z); pattern
`"yyyy-MM-dd HH:mm:ss Z"`, `Locale.US`; **no host default zone is read
anywhere**.

| k | rawOffset | id | at | HotSpot | tzdb-by-id (the wrong answer) |
|---|---|---|---|---|---|
| 0 | 0 | America/Sao_Paulo | JAN | `2021-01-15 12:00:00 +0000` | `2021-01-15 09:00:00 -0300` |
| 1 | 0 | America/New_York | JAN | `2021-01-15 12:00:00 +0000` | `2021-01-15 07:00:00 -0500` |
| 2 | 18000000 | America/Sao_Paulo | JUL | `2021-07-15 17:00:00 +0500` | `2021-07-15 09:00:00 -0300` |
| 3 | -18000000 | Europe/Berlin | JUL | `2021-07-15 07:00:00 -0500` | `2021-07-15 14:00:00 +0200` |
| 4 | 3600000 | Asia/Kolkata | JAN | `2021-01-15 13:00:00 +0100` | `2021-01-15 17:30:00 +0530` |
| 5 | -3600000 | Asia/Tokyo | JUL | `2021-07-15 11:00:00 -0100` | `2021-07-15 21:00:00 +0900` |
| 6 | 19800000 | UTC | JAN | `2021-01-15 17:30:00 +0530` | `2021-01-15 12:00:00 +0000` |
| 7 | 0 | Asia/Tokyo | JUL | `2021-07-15 12:00:00 +0000` | `2021-07-15 21:00:00 +0900` |
| 8 | -10800000 | Pacific/Kiritimati | JAN | `2021-01-15 09:00:00 -0300` | **`2021-01-16` 02:00:00 +1400** |
| 9 | 0 | Australia/Adelaide | JUL | `2021-07-15 12:00:00 +0000` | `2021-07-15 21:30:00 +0930` |

Rows 0–3 are C12-1 §3's four, unchanged. The six added rows carry two
deliberate properties:

* **Row 6 is the INVERSE of the classic vacuous idiom.** `(0, "UTC")` is the
  row that passes on a broken VM (and `(0, "Z")` is the near-universal real-world
  spelling of it), so the fixture carries `(19800000, "UTC")` instead: same
  label, non-vacuous.
* **Row 8's wrong answer lands on a DIFFERENT CALENDAR DAY.**
  `Pacific/Kiritimati` is UTC+14, the largest offset in tzdb, so a formatter
  that got only the clock field wrong would still be caught by the date.

Every expected string was measured, not remembered. Zone ids are all fixed, so
the numbers reproduce on any machine.

## 3. `routes` — a discriminator with no expected value at all

The intrinsic is registered for ONE descriptor. `SimpleDateFormat` does not
declare `format(Date)` (it is `DateFormat`'s), so the receiver-class lookup
misses, the climb runs, and the intrinsic wins. The other three routes run real
bytecode:

```
DateFormat.format(Date)Ljava/lang/String;                <- INTERCEPTED
DateFormat.format(Object)Ljava/lang/String;              <- real bytecode
SimpleDateFormat.format(Date,StringBuffer,FieldPosition) <- real bytecode, declared on the
                                                            receiver's own class
fmtBase(DateFormat f, ...)                               <- same method, DateFormat-typed call site
```

`routes` asserts the four **agree with each other**. HotSpot satisfies that
trivially; a VM with one wrong arm cannot. Two consequences worth stating:

* the block **survives every edit to the expected strings** in §2, so it stays
  sharp even if the tables rot;
* it **says which arm is wrong**. `fmtdate` red + `routes` red means the
  intrinsic. `fmtdate` red + `routes` green means both arms agree on a wrong
  answer — a different defect, and not this one.

The fourth route also settles C12-1 §2's claim behaviourally: dispatch keys on
the RECEIVER's runtime class, never the call site's static type, so a
`DateFormat`-typed call must not change the answer.

## 4. `roundtrip` — and the SECOND vacuity trap, which is easy to walk into

A round trip through `"yyyy-MM-dd HH:mm:ss Z"` proves nothing. `Z` writes the
offset into the text, so `parse` reads the offset back out of the STRING and
never consults the zone; **the round trip stays green with a completely broken
`format`.** `roundtrip` therefore uses `"yyyy-MM-dd HH:mm:ss"`, which has no
zone field: format takes the offset from the zone, parse takes it from the zone,
and only a VM where both agree round-trips.

That is exactly the half-landed state C12-1 predicts — parse answering from the
real `rawOffset` while format answers from tzdb — and §5's mutant B reproduces
it and shows the block catching it.

## 5. `parse` — the measured anchor, with the corrected number pinned

C6-2 §D predicted the bc-java skew as `10800000` ms (the zone's standard
offset). C12-1 §4 corrected it by measurement to `7200000`, because Sao Paulo
was observing daylight saving on 2002-01-22 and the tzdb path applies the
transition in force AT THE INSTANT. The fixture asserts both:

```java
check(skew == 7200000L,  "the Sao Paulo skew on 2002-01-22 must be 7,200,000 ms ...");
check(skew != 10800000L, "the skew must NOT be 10,800,000 ms (the zone's STANDARD offset).
                          An earlier record predicted that figure and measurement corrected
                          it; this check exists so the guess cannot come back");
```

The right number as the answer, the wrong one as an explicit negative, so nobody
re-derives the guess from the standard offset. The tzdb side is **computed**
(`TimeZone.getTimeZone("America/Sao_Paulo")`, the same computation
`zone_rules_cached` performs) rather than written as a literal, so the number
stays honest if tzdb moves.

C12-1's corollary is carried in the class comment because it governs anyone
adding rows: **the skew is a time series, not a constant**, so a block asserting
a fixed delta passes or fails on the date it picks. The four `parse` rows assert
absolute instants, never deltas.

Measured parse rows:

| text | zone | HotSpot | tzdb-by-id | skew |
|---|---|---|---|---|
| `20020122122220` | `(0, America/Sao_Paulo)` | `1011702140000` | `1011709340000` | `+7200000` |
| `20020122122220` | `(18000000, America/New_York)` | `1011684140000` | `1011720140000` | `+36000000` |
| `20020122122220` | `(-18000000, Europe/Berlin)` | `1011720140000` | `1011698540000` | `-21600000` |
| `20210715120000` | `(3600000, Asia/Kolkata)` | `1626346800000` | `1626330600000` | `-16200000` |

## 6. `dstrule` — the eleven-arg constructor, on the format path

C6-2 §B established that the longer constructors lose their explicit DST rule
the same way the two-arg one loses its `rawOffset`. A real US rule on
`rawOffset = -05:00`, labelled `"UTC"`, is the sharpest single object in the
file: the label resolves to a zone with NO daylight saving, so a VM answering
from the label formats both instants five hours away AND prints the SAME offset
in January and July, where the correct answer changes.

```
HotSpot: useDaylightTime=true getRawOffset=-18000000 offJAN=-18000000 offJUL=-14400000
         format(JAN) = 2021-01-15 07:00:00 -0500
         format(JUL) = 2021-07-15 08:00:00 -0400
tzdb-by-id ("UTC"):  both instants at +0000
```

The last check in the block is the offset-changes assertion, which needs no
expected value: `jan.substring(20)` must differ from `jul.substring(20)`.

## 7. The HotSpot transcript (the oracle)

```
$ javac -d out regression-suite/src/RSimpleDateFormatZone.java
$ java -cp out RSimpleDateFormatZone
CK RSimpleDateFormatZone control-kolkata=2021-01-15 17:30:00 +0530
CK RSimpleDateFormatZone control-gmtoffset=2021-01-15 17:30:00 +0530
CK RSimpleDateFormatZone control-utc=2021-01-15 12:00:00 +0000
CK RSimpleDateFormatZone control-newyork-dst=2021-07-15 08:00:00 -0400
CK RSimpleDateFormatZone control=8
CK RSimpleDateFormatZone fmt0=2021-01-15 12:00:00 +0000
CK RSimpleDateFormatZone fmt1=2021-01-15 12:00:00 +0000
CK RSimpleDateFormatZone fmt2=2021-07-15 17:00:00 +0500
CK RSimpleDateFormatZone fmt3=2021-07-15 07:00:00 -0500
CK RSimpleDateFormatZone fmt4=2021-01-15 13:00:00 +0100
CK RSimpleDateFormatZone fmt5=2021-07-15 11:00:00 -0100
CK RSimpleDateFormatZone fmt6=2021-01-15 17:30:00 +0530
CK RSimpleDateFormatZone fmt7=2021-07-15 12:00:00 +0000
CK RSimpleDateFormatZone fmt8=2021-01-15 09:00:00 -0300
CK RSimpleDateFormatZone fmt9=2021-07-15 12:00:00 +0000
CK RSimpleDateFormatZone fmtdate=20
CK RSimpleDateFormatZone routes=30
CK RSimpleDateFormatZone bare0=2021-01-15 12:00:00
CK RSimpleDateFormatZone bare1=2021-01-15 12:00:00
CK RSimpleDateFormatZone bare2=2021-07-15 17:00:00
CK RSimpleDateFormatZone bare3=2021-07-15 07:00:00
CK RSimpleDateFormatZone bare4=2021-01-15 13:00:00
CK RSimpleDateFormatZone bare5=2021-07-15 11:00:00
CK RSimpleDateFormatZone bare6=2021-01-15 17:30:00
CK RSimpleDateFormatZone bare7=2021-07-15 12:00:00
CK RSimpleDateFormatZone bare8=2021-01-15 09:00:00
CK RSimpleDateFormatZone bare9=2021-07-15 12:00:00
CK RSimpleDateFormatZone roundtrip=30
CK RSimpleDateFormatZone parse0=1011702140000
CK RSimpleDateFormatZone parse1=1011684140000
CK RSimpleDateFormatZone parse2=1011720140000
CK RSimpleDateFormatZone parse3=1626346800000
CK RSimpleDateFormatZone anchor-skew=7200000
CK RSimpleDateFormatZone parse=10
CK RSimpleDateFormatZone dstrule-jan=2021-01-15 07:00:00 -0500
CK RSimpleDateFormatZone dstrule-jul=2021-07-15 08:00:00 -0400
CK RSimpleDateFormatZone dstrule=11
CK RSimpleDateFormatZone checks=109
PASS RSimpleDateFormatZone (109 checks)
```

**Every formatted string is on a `CK` line**, so the evidence survives
`extract()` and reaches the cross-VM diff: a CratonVM that formats `fmt0` as
`2021-01-15 09:00:00 -0300` fails the diff even in the branch where its
assertion somehow does not throw.

Byte-identical over three consecutive runs. No line on a non-`PASS`/`CK`
prefix, so guard G1 is clean. Run against `harness-guard.sh`'s own functions:

```
RSimpleDateFormatZone: oracle_guard=0 extract_guard=0 count=109 lines=39
```

G1/G2/G3/G4 all clean. The class must NOT be added to `harness-uncounted.txt`.

## 8. MUTATION CHECK — three mutants

Every format goes through `fmt(SimpleDateFormat, Date)` and `fmtBase(DateFormat,
Date)`; every parse through `prs(SimpleDateFormat, String)`. Three seams, so a
mutant differs from the fixture by one method body.

### Mutant A — the FORMAT defect (`date_format_fast.rs:806`)

```java
static String fmt(SimpleDateFormat f, Date d) {
    SimpleDateFormat g = (SimpleDateFormat) f.clone();
    g.setTimeZone(TimeZone.getTimeZone(f.getTimeZone().getID()));   // resolve the ID FIELD
    return g.format(d);
}
```

```
  control : GREEN
  fmtdate : RED   fmtdate[0] new SimpleTimeZone(0, "America/Sao_Paulo").format(instant 1610712000000)
                  must be "2021-01-15 12:00:00 +0000", got "2021-01-15 09:00:00 -0300" -- the caller's
                  rawOffset was ignored and the zone's ID was resolved against tzdb instead
  routes : RED    routes[0]: format(Date)="2021-01-15 09:00:00 -0300" but
                  format(Object)="2021-01-15 12:00:00 +0000" -- the same formatter answered two
                  different things, so one of the two dispatch routes is not running the class library
  roundtrip : RED roundtrip[0] zone-less format of instant 1610712000000 under
                  SimpleTimeZone(0, "America/Sao_Paulo") must be "2021-01-15 12:00:00",
                  got "2021-01-15 09:00:00"
  parse : GREEN
  dstrule : RED   dstrule: format(JAN) must be "2021-01-15 07:00:00 -0500", got "2021-01-15 12:00:00 +0000"
```

`control` GREEN and `parse` GREEN are **correct and load-bearing**: the mutant
substitutes a `ZoneInfo` for a `ZoneInfo`, which is a no-op, and it does not
touch the parse seam. A `control` that went red would mean the fix over-reaches
into the population the fast arm is FOR.

### Mutant B — the PARSE defect (the four natives C12-1 §1 unregistered)

```java
static long prs(SimpleDateFormat f, String s) throws ParseException {
    SimpleDateFormat g = (SimpleDateFormat) f.clone();
    g.setTimeZone(TimeZone.getTimeZone(f.getTimeZone().getID()));
    return g.parse(s).getTime();
}
```

```
  control : GREEN
  fmtdate : GREEN
  routes : GREEN
  roundtrip : RED roundtrip[0]: format then parse under the SAME zone must return the original
                  instant 1610712000000, got 1610722800000 (delta 10800000 ms) -- format and parse
                  disagree about this zone's offset, which is the signature of a half-landed fix
  parse : RED     parse[0] "20020122122220" under SimpleTimeZone(0, "America/Sao_Paulo") must be
                  1011702140000, got 1011709340000 (delta 7200000 ms)
  dstrule : GREEN
```

**The `parse[0]` delta is exactly 7,200,000** — the corrected anchor, produced
by a mutant rather than asserted from a literal. And the two mutants are
DISJOINT in which blocks they redden, which is the property that makes the
suite able to distinguish a half-landed fix from a whole one.

### Mutant V — the vacuity guard itself

Row 1's `rawOffset` edited from `0` to `-18000000`, i.e. `America/New_York`'s
own standard offset — C6-2's named trap — with both expected strings updated to
match so the row would otherwise pass on a broken VM:

```
  control : GREEN
  fmtdate : RED   fmtdate[1]: VACUOUS VECTOR -- rawOffset -18000000 equals America/New_York's true
                  offset -18000000 at instant 1610712000000, so both the correct and the ID-resolved
                  implementation answer the same thing. Pick another (rawOffset, id) pair.
  routes : GREEN
  roundtrip : RED roundtrip[1]: VACUOUS VECTOR -- ...
  parse : GREEN
  dstrule : GREEN
```

**The trap that cost an earlier lane real time is now a red test, not a
comment.** (`routes` GREEN is right: it asserts route agreement, which a vacuous
row does not affect. That is the block being honest about what it measures.)

## NOMINATION — `regression-suite/run.sh` (this lane may not edit it)

`RSimpleDateFormatZone` belongs in **`CORE_CLASSES`**, beside
`RSimpleTimeZoneRaw`, for the reason C6-2 already gives: the accessor half of
this defect reproduced on `--jdk-only` AND `--real-jdk`, and the intrinsic is
registered in both arms, so it is a default-mode compatibility concern.

REPLACE (end of the `CORE_CLASSES` line, `run.sh:106`):

```
RImmutableFactoryTypes RJdkStringCodePoints"
```

WITH:

```
RImmutableFactoryTypes RJdkStringCodePoints RSimpleDateFormatZone"
```

**If C19-1's nomination is applied in the same edit**, the combined replacement
is:

```
RImmutableFactoryTypes RJdkStringCodePoints RJdkOptionalShape RSimpleDateFormatZone"
```

## Residuals

1. **`fmtdate` and `roundtrip` will both be red on a VM where only `format` is
   broken, and `roundtrip` will ALSO be red on a VM where only `parse` is
   broken.** That overlap is intentional (§4) but it means `roundtrip` alone
   does not attribute; read `fmtdate` and `parse` to locate the half.
2. **Locale-dependent zone DISPLAY names are not covered.** The patterns use
   `Z` (numeric offset) and never `z` or `zzzz`, because the display name is
   locale data and `P4A-CORPORA-20260812.md` §B records a separate, undiagnosed
   divergence there. Folding it in would make a red ambiguous.
3. **`getTimeZone(id)` returning `sun.util.calendar.ZoneInfo` is NOT asserted.**
   C12-1 3A moves CratonVM to that class, and C6-2 §4C shows HotSpot always
   returns it, but the `control` block asserts OFFSETS rather than the class
   name so that a red there is a behaviour finding rather than an identity one.
   Someone who wants the identity pinned should add it as its own family.
4. **The fixture was validated on Windows only.** No host default zone is read,
   so the numbers are host-independent by construction, but the Linux run has
   not been executed.
5. **This record asserts nothing about CratonVM.** A red in `fmtdate` /
   `routes` / `dstrule` is C12-1 §5 still being unapplied; a red in `parse` is a
   regression of C12-1 §1; a red in `control` means a fix over-reached.
