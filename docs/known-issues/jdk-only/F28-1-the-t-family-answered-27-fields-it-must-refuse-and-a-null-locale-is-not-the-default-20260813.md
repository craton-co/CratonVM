# F28-1 — the `%t` family answered 27 fields it must refuse, an explicit `null` locale is `Locale.US` and not the default, and every `%t` number was ASCII

**Date:** 2026-08-13
**Lane:** F28 (`--jdk-only` pool)
**File changed:** `native-builtins/src/lang_string.rs` (the lane's only owned file)
**Oracle:** HotSpot 25.0.3+9-LTS (`openjdk 25.0.3 2026-04-21 LTS`,
Microsoft-13877124).
**Zones/locales measured under:** `-Duser.timezone=Asia/Kolkata` throughout the
`%t` rows (a UTC host cannot see any of them); host default locale `ru_RU`, and
`-Duser.language=tr -Duser.country=TR -Duser.timezone=Europe/Istanbul` for the
locale rows. Every expectation below was MEASURED before it was written.
Nothing here is a predicted *behaviour*; the only predicted claims are marked
PREDICTED and concern which checks flip.

Continues F22-1 (`F22-1-formatter-utf16-units-and-t-zone-20260813.md`), whose
§5.2/§5.3/§5.4 are the first three sections here.

---

## 1. Where the brief was too narrow, and how far

The brief asked for **`%tZ` on `java.time` types** ("`Instant` and
`LocalDateTime` should throw on `%tZ`"). That is true and it is roughly one
thirtieth of the defect.

I swept **all 31 `%t` fields × all 6 `java.time` source types** on HotSpot 25
(186 cells). The refusal is not a `%tZ` rule at all — it is the JDK's
`print(Formatter, TemporalAccessor, char, Locale)` calling `t.get(ChronoField)`
and turning the resulting `DateTimeException` into
`IllegalFormatConversionException(c, t.getClass())`. Every field the source
does not support refuses:

| source | fields ANSWERED | fields REFUSED |
|---|---|---|
| `Instant` | 4 — `%tL %tN %ts %tQ` | **27** |
| `LocalDate` | 14 | 17 |
| `LocalTime` | 12 | 19 |
| `LocalDateTime` | 26 | 5 — `%tz %tZ %ts %tQ %tc` |
| `ZonedDateTime` | 31 | 0 |
| `OffsetDateTime` | 31 | 0 |

CratonVM answered **all 31 for all six**, because `extract_temporal_fields`'
`invoke_i32` returns `0` for a method the class does not have. So `%tH` of an
`Instant` printed a perfectly plausible `22` where HotSpot raises
`IllegalFormatConversionException: H != java.time.Instant`. **A fabricated
answer, not a missing feature** — the same species as the "`_ =>` default"
record: a helper whose fallback returns a plausible value is untested until
something reads it.

That is the difference between the brief's framing and the measurement: it is
not that two types refuse one conversion, it is that four of six types refuse
most conversions, and the two that refuse nothing are the two the brief said to
give offsets to.

---

## 2. Measured HotSpot 25 rows

### 2.1 The `java.time` × field matrix

`String.format(Locale.ROOT, "%t"+f, src)` at `1699999999000L`, under
`-Duser.timezone=Asia/Kolkata`. `THROW` is
`java.util.IllegalFormatConversionException`, and the message is
`<reported char> != <class name>`.

| field(s) | `Instant` | `LocalDate` | `LocalTime` | `LocalDateTime` | `ZonedDateTime` (Tokyo) | `OffsetDateTime` (-3) |
|---|---|---|---|---|---|---|
| `%tH` | THROW `H` | THROW `H` | `22` | `22` | `07` | `19` |
| `%tk` | THROW `k` | THROW `k` | `22` | `22` | `7` | `19` |
| `%tI` | THROW `I` | THROW `I` | `10` | `10` | `07` | `07` |
| `%tl` | THROW `l` | THROW `l` | `10` | `10` | `7` | `7` |
| `%tM` `%tS` | THROW | THROW | ok | ok | ok | ok |
| `%tp` | THROW `p` | THROW `p` | `pm` | `pm` | `am` | `pm` |
| `%tL` | `000` | THROW `L` | `000` | `000` | `000` | `000` |
| `%tN` | `000000000` | THROW `N` | ok | ok | ok | ok |
| `%ts` | `1699999999` | THROW `s` | THROW `s` | **THROW `s`** | `1699999999` | `1699999999` |
| `%tQ` | `1699999999000` | THROW `Q` | THROW `Q` | **THROW `Q`** | ok | ok |
| `%tz` | THROW `z` | THROW `z` | THROW `z` | **THROW `z`** | `+0900` | `-0300` |
| `%tZ` | THROW `Z` | THROW `Z` | THROW `Z` | **THROW `Z`** | `GMT+09:00` | `-03:00` |
| `%tB` `%tb` `%th` | THROW | `Nov` | THROW | `Nov` | `Nov` | `Nov` |
| `%tA` `%ta` | THROW | `Tue` | THROW | `Tue` | `Wed` | `Tue` |
| `%tC` `%tY` `%ty` `%tj` `%tm` `%td` `%te` | THROW | ok | THROW | ok | ok | ok |
| `%tR` `%tT` | THROW **`H`** | THROW **`H`** | ok | ok | ok | ok |
| `%tr` | THROW **`I`** | THROW **`I`** | ok | ok | ok | ok |
| `%tD` | THROW **`m`** | `11/14/23` | THROW **`m`** | ok | ok | ok |
| `%tF` | THROW **`F`** | `2023-11-14` | THROW **`F`** | ok | ok | ok |
| `%tc` | THROW **`a`** | THROW **`H`** | THROW **`a`** | THROW **`Z`** | `Wed Nov 15 07:13:19 GMT+09:00 2023` | `Tue Nov 14 19:13:19 -03:00 2023` |

**The bolded characters are the finding inside the finding.** A composite does
not report the character that was written: it delegates to nested `print`
calls and the INNER field raises. `%tT` of a `LocalDate` reports `H`, `%tc` of
an `Instant` reports `a`, of a `LocalDate` `H`, of a `LocalDateTime` `Z` —
three different characters for one conversion, selected by which group is
missing. `%tF` is the one composite that reports its own character, because
`ISO_STANDARD_DATE` reads its year with `t.get(yearField)` inline instead of
delegating. That is read in `Formatter.java` AND confirmed by the `Instant` and
`LocalTime` rows.

Note also `%tB` under `Locale.ROOT` is `Nov`, not `November`: CLDR's root
locale month names are the abbreviated forms. That is a property of the locale
data, not of the conversion, and this lane changed nothing about it.

### 2.2 `%tZ`/`%tz` on the two offset-carrying types

At `1699999999000L`, `-Duser.timezone=Asia/Kolkata` (whose own `+0530` appears
in none of these):

| argument | `Locale.ROOT` | `Locale.US` | `(Locale) null` | `%tz` |
|---|---|---|---|---|
| `atZone("Asia/Tokyo")` | `GMT+09:00` | `JST` | `JST` | `+0900` |
| `atZone("Asia/Kolkata")` | `GMT+05:30` | `IST` | | `+0530` |
| `atZone("UTC")` | `UTC` | | | |
| `atZone(ZoneOffset.ofHours(5))` | `+05:00` | | | |
| `atOffset(ZoneOffset.UTC)` | **`Z`** | | | `+0000` |
| `atOffset(-3)` | `-03:00` | | | `-0300` |
| `atOffset(-3,-30)` | `-03:30` | | | `-0330` |
| `atZone("America/New_York")`, **July** | | `EDT` | | `-0400` |
| `atZone("America/New_York")`, November | | `EST` | | `-0500` |

The `Z` row is why the id is taken off the object and never composed from the
offset: `ZoneOffset.UTC.getId()` is the single letter `Z`, and the others are
colon-separated where `%tz` is not. The July/November New York pair is the one
that separates a real daylight lookup from a hard-coded `false`.

The JDK rule, read in `Formatter.java`'s `DateTime.ZONE` case rather than
inferred from the table:

```java
ZoneId zid = t.query(TemporalQueries.zone());
if (zid == null) throw new IllegalFormatConversionException(c, t.getClass());
if (!(zid instanceof ZoneOffset) && t.isSupported(ChronoField.INSTANT_SECONDS)) {
    Instant instant = Instant.from(t);
    sb.append(TimeZone.getTimeZone(zid.getId())
                      .getDisplayName(zid.getRules().isDaylightSavings(instant),
                                      TimeZone.SHORT,
                                      Objects.requireNonNullElse(l, Locale.US)));
    break;
}
sb.append(zid.getId());
```

### 2.3 The `Given(None)` locale — SETTLED, on a non-US host

F22 could not settle this and said so; it needs a host whose default locale is
not `en_US`. This host's default is `ru_RU`, and forcing `tr_TR` gives the
sharper rows. `NUL` below is `(Locale) null`.

| expression | `tr_TR` default | `ru_RU` default | verdict |
|---|---|---|---|
| `format(NUL, "%tb", d)` | `Nov` | `Nov` | **`Locale.US`** |
| `format("%tb", d)` | `Kas` | `нояб.` | default |
| `format(NUL, "%tA", d)` | `Wednesday` | `Wednesday` | **`Locale.US`** |
| `format(NUL, "%tp", d)` | `am` | `am` | **English literal** |
| `format("%tp", d)` | `öö` | `am` | default |
| `format(NUL, "%tc", d)` | `Wed Nov 15 01:13:19 GMT+03:00 2023` | `Wed Nov 15 03:43:19 IST 2023` | **`Locale.US`** |
| `format("%tc", d)` | `Çar Kas 15 01:13:19 GMT+03:00 2023` | `ср нояб. 15 03:43:19 IST 2023` | default |
| `format(NUL, "%,d", 1234567)` | `1,234,567` | `1,234,567` | **ASCII constants** |
| `format("%,d", 1234567)` | `1.234.567` | `1` U+00A0 `234` U+00A0 `567` | default |
| `format(NUL, "%.2f", 1234.5)` | `1234.50` | `1234.50` | **ASCII constants** |
| `format(NUL, "%S", "i")` | **`İ`** (U+0130) | `I` | **DEFAULT locale** |
| `format(NUL, "%tr", d)` | `01:13:19 AM` | `03:43:19 AM` | English + default upper-caser |

So a null `Locale` means **three different things** in one class, and the JDK
means all three deliberately. Read in `Formatter.java`:

* `getZero` / `getDecimalSeparator` / `getGroupingSeparator` / `getMinusSign`:
  `locale == null ? '0'/'.'/','/'-' : …` — **ASCII constants, no lookup**.
* every name field and `%tZ`: `Objects.requireNonNullElse(l, Locale.US)` —
  **`Locale.US`**.
* `toUpperCaseWithLocale` (the `%S`/`%C`/`%B`/`%H` and `%T` upper-caser, and
  `%tr`'s marker): `s.toUpperCase(Objects.requireNonNullElse(l,
  Locale.getDefault(Locale.Category.FORMAT)))` — **the DEFAULT**.
* `%tp`'s array: `String[] ampm = {"AM","PM"}; if (l != null && l != Locale.US)
  { … dfs.getAmPmStrings(); }` then `.toLowerCase(requireNonNullElse(l,
  default))` — English literals, default-locale lower-caser.

Against that, CratonVM before this lane:

| site | CratonVM `Given(None)` | JDK | |
|---|---|---|---|
| `fmt_symbols_for` | ASCII constants | ASCII constants | **already right** |
| `fmt_upper_case` | DEFAULT locale | DEFAULT locale | **already right** |
| `fmt_date_name` | DEFAULT locale | `Locale.US` | **WRONG** |
| `fmt_zone_display_name` | DEFAULT locale | `Locale.US` | **WRONG** |

F22's brief-level summary — "an explicit `null` locale takes the default
locale's rules for case mapping but not for separators" — is CORRECT as far as
it goes, and it omits the third rule, which is the one that was wrong here.

### 2.4 Every `%t` NUMBER takes the locale's zero digit

Not in the brief at all; found while reading `localizedMagnitude` for §2.3.
Under `ar-EG` (zero digit U+0660), `new Date(1699999999000L)`,
`-Duser.timezone=Asia/Kolkata`. Dumped as hex code units:

| conversion | HotSpot 25 under `ar-EG` |
|---|---|
| `%tH` | `0660 0663` |
| `%tT` | `0660 0663` **`003A`** `0664 0663` **`003A`** `0661 0669` |
| `%tD` | `0661 0661` **`002F`** `0661 0665` **`002F`** `0662 0663` |
| `%tz` | **`002B`** `0660 0665 0663 0660` |
| `%tZ` | `0049 0053 0054` — **ASCII `IST`, not localized** |
| `%tp` | `0635` — a `DateFormatSymbols` name |
| `%tb` | `0646 0648 0641 0645 0628 0631` — a name |
| `%ts` `%tQ` `%tY` `%tj` `%td` `%te` `%tL` `%tN` `%tC` `%ty` `%tm` `%tI` `%tk` `%tl` | all Arabic-Indic digits |
| `%tc` | Arabic weekday + Arabic month + **Arabic-Indic** day/clock + **ASCII `IST`** + **Arabic-Indic** year |
| `%tr` | Arabic-Indic clock + ASCII `':'` + the Arabic marker `0635` |
| `[%10tH]` | `005B` + **eight `0020`** + `0660 0663` + `005D` — the pad is ASCII and runs AFTER |

CratonVM emitted ASCII digits for all of them. `java.util.logging`'s default
`SimpleFormatter` pattern opens `%1$tb %1$td, %1$tY` — so this is every log
line on such a host, the same blast radius the English-month-names defect had
(W7-91).

### 2.5 `%tF`'s year is not `%tY`'s

`Locale.ROOT`, `LocalDate` arguments:

| argument | `%tF` | `%tY` | `%ty` | `%tC` |
|---|---|---|---|---|
| `of(12345,3,4)` | **`+12345`**`-03-04` | `12345` | `45` | `123` |
| `of(9999,12,31)` | `9999-12-31` | `9999` | `99` | `99` |
| `of(-44,3,15)` | **`-0044`**`-03-15` | **`0045`** | `45` | `00` |
| `of(-1,6,5)` | `-0001-06-05` | **`0002`** | `02` | `00` |
| `of(0,2,29)` | `0000-02-29` | **`0001`** | `01` | `00` |

Two separate rules in one row set. `%tF` renders the sign OUTSIDE a four-digit
zero pad and emits a literal `'+'` past 9999; Rust's `{:04}` counts the sign
inside the width (`-044`) and has no `'+'` rule. And `%tY`/`%ty`/`%tC` read
`YEAR_OF_ERA` while `%tF` reads `YEAR`, which is why 45 BC is `0045` in one
column and `-0044` in another. **Only the first is fixed here** — see §5.4.

### 2.6 A `Formattable` that emits a lone surrogate

A `Formattable` whose `formatTo` does `f.format("%s", s)`:

| `s` | conversion | HotSpot 25 |
|---|---|---|
| `"x\uD800y"` | `%s` | len 3 — `0078 D800 0079` |
| `"x\uD800y"` | `%S` | len 3 — `0078 D800 0079` |
| `"x\uD800y"` | `%-10s` | len 3 — `0078 D800 0079` |
| `"\uDC00"` | `%s` | len 1 — `DC00` |
| `"😀"` | `%.1s` | len 2 — `D83D DE00` |
| `"x\uD800y"` NOT via `Formattable` | `%s` | len 3 — `0078 D800 0079` |

The `%S` row is a second measurement, not decoration: the `x` survives as
lower-case `0078`, which proves the upper-caser does not run on a
`Formattable`'s output; the `%-10s` row proves the justifier does not; the
`%.1s` row proves the precision does not. CratonVM's answer was
`0078 FFFD 0079` — the loss is at the READ of the callee's `StringBuilder`, and
nowhere else.

---

## 3. What changed

All in `native-builtins/src/lang_string.rs`.

### 3.1 `Formattable` output is read as code units (F22 §5.4)

`fmt_formattable_dispatch` returns `Vec<u16>`; its `toString()` read is
`read_string_chars`, not `ctx.read_string`. The single call site in
`format_arg_full` returns those units directly instead of
`out.encode_utf16()`. **This is F22's own one-line diagnosis, confirmed** —
"the loss is at that READ, not in the pipeline".

### 3.2 `FmtSupport`, and a per-FIELD refusal

New `FmtSupport { date, time, sub_second, instant, zone }` — the five field
GROUPS the JDK's own `switch` partitions on, **not a per-type table**, so a
seventh source type does not need the matrix re-derived. `extract_temporal_fields`
now returns it alongside the fields and the zone; each `java.time` arm names
its groups; the four epoch-shaped sources take `FmtSupport::ALL` because they
go through the `Calendar` printer, which refuses nothing.

New `fmt_temporal_fault_char(field, support) -> Option<char>` maps a field to
the character the exception reports, including the composites' inner-field
rule. `format_temporal_field` consults it immediately after decoding and
raises `FmtFault::WrongType(c, cid)` — which is already
`IllegalFormatConversionException(char, Class)` with the JDK's own
`"<c> != <class>"` message, so no new exception plumbing was added.

Screening in ONE place rather than in 31 arms is deliberate: the decoded
fields are, for an unsupported field, fabrications, and nothing downstream may
see them.

### 3.3 `%tZ`/`%tz` on `ZonedDateTime` / `OffsetDateTime` (F22 §5.2)

* `FmtZone` gains `temporal: bool`. Its doc now spells out that
  `known == false` is **not** the same question as "must refuse": a
  `LocalDateTime` has no zone and must throw, a VM that cannot reach
  `java.util.TimeZone` has no zone and must keep printing `UTC`. The first is
  `FmtSupport::zone == false`, the second is `FmtZone::known == false`.
* New `fmt_temporal_zone` reads `getOffset().getTotalSeconds()` — which IS
  `ChronoField.OFFSET_SECONDS`, the quantity `%tz` renders — off the object.
* New `fmt_temporal_zone_name` implements the `DateTime.ZONE` rule in §2.2: a
  fixed-offset `ZoneId` prints `getId()`; a region zone goes through
  `TimeZone.getTimeZone(ZoneId)` and the short display name. Which method to
  call is decided by a CLASS test (`ZonedDateTime` has `getZone`,
  `OffsetDateTime` does not), not by trying the call and discarding a
  `NoSuchMethodError` on every `%tZ`.
* The daylight flag reuses `fmt_resolve_zone`'s rule
  (`getOffset(millis) != getRawOffset()`) rather than `ZoneRules
  .isDaylightSavings`, so the two `%tZ` routes ask the daylight question in one
  form. The July/November New York rows are the pair that covers it.
* Fallback is `fmt_zone_gmt_form(offset)`, which IS HotSpot's own `Locale.ROOT`
  answer for a region zone — the `GMT+09:00` cell above.

### 3.4 `Given(None)` is `Locale.US` (F22 §5.3)

* `fmt_date_name` returns `None` for `Given(None)`, **before the latch and
  before any bytecode**, and the caller prints its English table. Those four
  tables ARE `DateFormatSymbols.getInstance(Locale.US)`'s answers for all five
  getters, which is also why `printDateTime`'s own `AM_PM` arm skips
  `DateFormatSymbols` for a null. Asking the JDK for US data would be a slower
  route to the same characters with a re-entrancy hazard the early return does
  not have.
* `fmt_zone_display_name`'s locale handling moved into a new shared
  `fmt_zone_name_of`, so the two `%tZ` routes cannot answer the locale question
  two ways. `Given(None)` there resolves `Locale.US` through a new
  `fmt_locale_us` — a static-field READ, degrading to the previous two-argument
  `getDisplayName` if `java.util.Locale` is absent or uninitialized. Nothing
  fabricates a `Locale`; a wrong one would rename every zone.
* `fmt_upper_case` and `fmt_symbols_for` were **read and left exactly as they
  were**, and their comments were verified against the source. F22 flagged that
  they "deliberately disagree"; they do, and §2.3 shows the JDK disagrees with
  itself in the same direction, so both are right.

**What this costs, stated plainly.** In this VM `Given(None)` is also produced
by `new Formatter()` / `new Formatter(Appendable)`, whose natives write `null`
into the locale slot (§5.1). For those the right answer is the DEFAULT locale,
and `fmt_date_name` now gets them wrong. That is not a new trade — it is the
trade `fmt_symbols_for` has always made on that same path — so the change makes
the file self-consistent and collapses the whole residual into one constructor
fix instead of three consumers. W7-34 already carries that constructor row as
open.

### 3.5 `%t` numbers take the locale's zero digit; `%tF`'s year rule

* New `fmt_localize_digits` — `fmt_localize`'s digit half. Deliberately NOT
  `fmt_localize` with the separators zeroed: a `%t` field carries no grouping
  separator and no decimal point (its `localizedMagnitude` calls pass
  `Flags.NONE`/`Flags.ZERO_PAD`, never `Flags.GROUP`), and mapping `'.'` would
  be actively wrong — `%tD` is `mm/dd/yy` and `ru`'s `%tb` is `нояб.`.
* `format_temporal_field` takes `FmtSymbols` and applies it to every field
  except the seven pure-NAME ones; `%tc` and `%tr` localize their numeric slots
  individually, because localizing their composed output would rewrite the
  digits inside a `GMT+05:30` zone name.
* `format_impl` resolves the symbols for `%t` through the **same per-call cache
  the numeric conversions already use**, and skips it entirely for the seven
  name fields — so a `%tb`-only format string still runs no
  `DecimalFormatSymbols` bytecode.
* New `fmt_iso_year` implements `%tF`'s sign-outside-the-pad rule.

---

## 4. Verified vs. assumed

**Verified (measured on HotSpot 25, or read in `C:\craton\jdk25src`):**

* Every cell in §2.1–§2.6. The zone/locale each was taken under is stated
  above; on a UTC, en-US host most of them are invisible.
* The `DateTime.ZONE` source, the four null-locale rules, `localizedMagnitude`,
  `ISO_STANDARD_DATE`'s year block, and the `catch (DateTimeException x) ->
  IllegalFormatConversionException(c, t.getClass())` that produces the whole
  refusal matrix — all read in `java.base/java/util/Formatter.java`.
* That `native-builtins/src/lib.rs`'s `java/util/Formatter` `<init>()V` and
  `<init>(Ljava/lang/Appendable;)V` write `Value::Object(None)` into slot 1,
  that `format` reads slot 1 and hands it to
  `lang_string::native_string_format_locale`, and therefore that `Given(None)`
  genuinely is two requests at once. **The premise F22's `fmt_date_name`
  comment was scoped by is real and still live** — it was checked, not assumed
  away, and §5.1 is what it turns into.
* That the file's own `FmtFault::WrongType` already builds
  `IllegalFormatConversionException(char, Class)` with the `"c != name"`
  message.
* `rustfmt` on a COPY: the file parses, and the four blocks rustfmt wanted
  reflowed were reflowed. Line endings verified LF (`tr -cd '\r' | wc -c` = 0)
  after every edit.

**Assumed / PREDICTED (no build, no test run, no VM execution — lane
constraint):**

* That the crate compiles. Every changed signature's call sites were updated
  and each is unique: `fmt_formattable_dispatch` 1, `extract_temporal_fields`
  1, `format_temporal_field` 1. **This is unverified by a compiler.**
* PREDICTED — checks that **flip** (were wrong, now right): `%tZ`/`%tz` on
  `ZonedDateTime`/`OffsetDateTime`; a `Formattable` emitting a lone surrogate;
  `%tb`/`%tB`/`%ta`/`%tA`/`%tp`/`%tZ`/`%tc`/`%tr` under an explicit null locale
  on a non-US host; every `%t` number under a non-Latin-digit locale; `%tF` of
  a negative or five-digit year.
* PREDICTED — checks that **become reachable** (were answered, now refuse): the
  186-cell matrix's 79 refusing cells. These are behaviour CHANGES, not just
  corrections: code that formatted `%tH` of an `Instant` and got `22` now gets
  an exception. HotSpot is the oracle and HotSpot throws, but this is the one
  part of the change with a blast radius, and it is the reason it is called out
  here rather than buried in §3.
* PREDICTED — **nothing else moves**. The epoch-shaped sources take
  `FmtSupport::ALL` and refuse nothing; `fmt_localize_digits` short-circuits on
  `zero == '0'`, which is every Latin-digit locale; `fmt_iso_year` is identical
  to `{:04}` for `1..=9999`.

**Reachable, not flipped.** `FmtFault::WrongType`, `fmt_zone_gmt_form` and
`sb_string_from_units` were already correct; what changed is that the `%t`
refusal path, the `java.time` zone path and the `Formattable` path now reach
them.

---

## 5. NOMINATIONS

### 5.1 OPEN, and it closes §3.4's residual — `native-builtins/src/lib.rs`, the two `Formatter` constructors that write a null locale

The real-JDK registrar's `java/util/Formatter` `<init>()V` and
`<init>(Ljava/lang/Appendable;)V` write `null` into the locale slot where the
real `java.util.Formatter()` constructor writes
`Locale.getDefault(Locale.Category.FORMAT)`. `format` then reads slot 1 and
hands it on, so `new Formatter(sb).format("%tb", d)` arrives in
`lang_string.rs` as `FmtLocale::Given(None)` — an EXPLICIT null — and takes
`Locale.US` name data and ASCII separators where HotSpot takes the default
locale's.

W7-34 already lists this as open ("the `new Formatter()` half of this row").
It is now also what §3.4's trade is paid for.

**File:** `native-builtins/src/lib.rs`, ≈21439, inside
`registry.register(f, "<init>", "()V", …)`
*exact literal old text:*
```rust
        let empty = ctx.create_string("");
        ctx.set_field(this, 0, Value::Object(Some(empty)));
        ctx.set_field(this, 1, Value::Object(None));
        Ok(None)
    });
    registry.register(f, "<init>", "(Ljava/util/Locale;)V", |ctx, args| {
```
*exact literal new text:*
```rust
        let empty = ctx.create_string("");
        ctx.set_field(this, 0, Value::Object(Some(empty)));
        // The real `java.util.Formatter()` is
        // `this(Locale.getDefault(Locale.Category.FORMAT), new StringBuilder())`,
        // so slot 1 is the DEFAULT locale, not null. A null here is read by
        // `lang_string::native_string_format_locale` as an EXPLICIT null
        // locale, whose JDK rules are different in three places: ASCII
        // separators, `Locale.US` date names and `Locale.US` zone names.
        // Measured on HotSpot 25 with the host default forced to `tr_TR`,
        // `new Formatter().format("%tb", d).toString()` is `Kas`, and
        // `format((Locale) null, "%tb", d)` is `Nov`. F28-1 §2.3.
        ctx.set_field(this, 1, fmt_default_locale_value(ctx));
        Ok(None)
    });
    registry.register(f, "<init>", "(Ljava/util/Locale;)V", |ctx, args| {
```

**File:** `native-builtins/src/lib.rs`, ≈21462, inside
`registry.register(f, "<init>", "(Ljava/lang/Appendable;)V", …)`
*exact literal old text:*
```rust
        let appendable = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, appendable);
        ctx.set_field(this, 1, Value::Object(None));
        Ok(None)
    });
    // `Formatter(Appendable, Locale)` is deliberately NOT registered here,
```
*exact literal new text:*
```rust
        let appendable = args.get(1).copied().unwrap_or(Value::Object(None));
        ctx.set_field(this, 0, appendable);
        // See the `()V` constructor above: slot 1 is the DEFAULT locale.
        ctx.set_field(this, 1, fmt_default_locale_value(ctx));
        Ok(None)
    });
    // `Formatter(Appendable, Locale)` is deliberately NOT registered here,
```

plus one new helper in the same file (placement is the owner's call):
```rust
/// `Locale.getDefault()` as a `Value`, or a null one if it cannot be resolved.
///
/// The JDK's `Formatter` constructors take
/// `Locale.getDefault(Locale.Category.FORMAT)`; this asks for the no-argument
/// default, which is what every other default-locale resolution in
/// `lang_string.rs` asks for (`fmt_formattable_dispatch`, and
/// `DecimalFormatSymbols.getInstance()`'s own body). The two differ only on a
/// host that has set the FORMAT category apart from the display category.
fn fmt_default_locale_value(ctx: &mut dyn NativeContext) -> Value {
    match ctx.invoke("java/util/Locale", "getDefault", "()Ljava/util/Locale;", &[]) {
        Ok(Some(v @ Value::Object(Some(_)))) => v,
        _ => Value::Object(None),
    }
}
```

**The same two writes exist in the synthetic-mode registrar** —
`native_formatter_init` (≈42728) and `native_formatter_init_appendable`
(≈42742), both `ctx.set_field(this, 1, Value::Object(None));`. Fix them
together or the two modes disagree; note the string appears **four** times in
that file, so match on the surrounding lines, not on it alone.

**Alternative the file's own reasoning points at, NOT taken here.** The
comment beside these registrations already argues that
`Formatter(Appendable, Locale)` is deliberately unregistered *because the real
constructor writes both slots correctly plus `zero`*. That argument applies
verbatim to `()V` and `(Appendable)V`, and DELETING both registrations would be
the smaller, more principled change. It is not nominated as the primary because
deleting a registration changes which code writes slot 0 as well, and this lane
cannot build or run to check that. Whoever owns the file should weigh it.

### 5.2 OPEN — `%tY`/`%ty`/`%tC` read the proleptic year, not `YEAR_OF_ERA`

§2.5: `%tY` of `LocalDate.of(-44,3,15)` is `0045` on HotSpot (45 BC) and
`%tF` of the same date is `-0044`. CratonVM renders the proleptic year for
both. Only `%tF` is fixed here.

Left out because the era rule is not a one-liner across the sources: for a
`java.time` argument `YEAR_OF_ERA` is `1 - year` when `year <= 0`, but a
`Calendar` argument's `Calendar.YEAR` is ALREADY era-relative and carries a
separate `ERA` field, so a single expression over the decoded proleptic year
would fix one source and break the other. Measured `GregorianCalendar` rows are
in §2.5. It needs its own lane and its own `Calendar`-side measurements.

### 5.3 OPEN — no `DecimalFormatSymbols.getMinusSign()` anywhere in this file

`%tF`'s minus, and every negative `%d`/`%f`/`%e`/`%g`, use the ASCII `'-'`;
the JDK uses `getMinusSign(l)`. `FmtSymbols` carries three symbols and this
would be a fourth on the hot path. Measured: `ar-EG`'s minus sign IS U+002D, so
the locale most likely to expose it does not. Recorded rather than guessed at —
finding a locale where it differs is the first step, not writing the code.

### 5.4 OPEN — `%tp`'s lower-caser is Rust's, not the locale's

`'p' => fmt_date_name(…).map(|s| s.to_lowercase())`. The JDK is
`s.toLowerCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT)))`.
This is the exact mirror of the N3 defect W8-F4-1 recorded on the UPPER-case
side, which was fixed by routing through `case_map`; the lower-case twin was
not. `case_map` already owns the `tr`/`az`/`lt` rules and
`crate::locale_language_for_case_mapping` already resolves the language, so the
fix is [`fmt_upper_case`]-shaped. NOT MEASURED as a live divergence: I did not
find a locale whose AM/PM strings contain a character the two rules map
differently, and I am not claiming one exists. It is a one-rule-two-
implementations note, not a measured row.

---

## 6. Left undone

* No build, no test run, no VM execution — lane constraint. §4 lists what that
  leaves unverified.
* §5.1–§5.4.
* The 79 refusing cells in §2.1 are asserted through `fmt_temporal_fault_char`
  as a pure function; there is no end-to-end vector in `regression-suite` for
  them, and adding one is a different file.
* The `java.time` sources still decode through per-class getter calls rather
  than `ChronoField`; `FmtSupport` describes the JDK's partition but does not
  make the VM ASK the object which fields it supports. A `TemporalAccessor
  .isSupported` route would be the structurally correct one and is a bigger
  change than this lane's brief.
