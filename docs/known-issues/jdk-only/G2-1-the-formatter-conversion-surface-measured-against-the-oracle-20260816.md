# G2-1 — the `java.util.Formatter` conversion surface measured against the oracle, and the four things that disagreed with it

**Date:** 2026-08-16
**Lane:** G2 (`--jdk-only` pool), branch `claude/jdk-only-mode-completion-1351c0`
**File changed:** `native-builtins/src/lang_string.rs` — this lane's only owned file.
**Oracle:** HotSpot 25.0.3+9-LTS, Eclipse Adoptium, at
`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`, single-file source
mode (`java FmtProbe.java`), `-Dfile.encoding=UTF-8`.

---

## 0. Provenance, stated once and plainly

**Every HotSpot number in this record is MEASURED.** 2,932 cells were run
across three probes (1,059 + 1,445 + 428), all reproduced below with their
sources.

**Every CratonVM number in this record is PREDICTED.** This lane cannot build
and cannot run the VM: the tree is mid-merge, there is no binary, and the
orchestrator owns `cargo`. Every "CratonVM before" below was obtained by
*reading* `lang_string.rs`, and every "CratonVM after" is what the edited code
*should* produce. The directory's recurring sin is predictions that read like
results; this section is the whole of the defence against it. The orchestrator
must treat §2's after-columns as hypotheses until a binary has answered them.

**What that means for §2's four defects.** Each names the exact line of
reasoning that produced the "before" value, so a reviewer can falsify it by
reading rather than by building. Three of the four are one-line reads; the
fourth (§2.3) is an absent `else if` arm, which is as close to unfalsifiable-
by-reading as this kind of claim gets.

---

## 1. Two earlier records claimed edits that "landed, unbuilt". Both really did land

The brief asked whether `W8-F4-1`'s and `W8-F13-1`'s edits are present in the
tree, absent, or present-but-inert — the HANDOFF's §5 records two separate
occasions where a change compiled and did nothing. Read at HEAD `e9d95f66b`:

| record | claim | present in the tree? | reachable? |
|---|---|---|---|
| `W8-F4-1` `%h` is `Integer.toHexString(arg.hashCode())` | yes | **PRESENT**, `lang_string.rs` `format_arg`, the `spec == 'h' \|\| spec == 'H'` arm invokes `hashCode` virtually and formats `{:x}` of `hash as u32` | **REACHABLE** — `fmt_general_units` calls `format_arg` for `'h'`, and `format_arg_full` routes `'h'`/`'H'` through the general branch |
| `W8-F4-1` `'H'` added to the upper-case remap table | yes | **PRESENT**, `format_arg_full`'s `match spec { 'S' => ('s',true), 'B' => ('b',true), 'C' => ('c',true), 'H' => ('h',true), … }` | reachable |
| `W8-F4-1` the null argument takes the JDK's shared printer | yes | **PRESENT**, the `matches!(val, Value::Object(None))` branch in `format_arg_full`: truncate → upper-case → space-justify | reachable; `format_arg`'s own null arm is now dead and routed through `fmt_null_text` |
| `W8-F13-1` `%s` of a `Formattable` dispatches | yes | **PRESENT**, `fmt_formattable_dispatch` (≈`lang_string.rs:6737`), called from `format_arg_full` under `if spec == 's'` after the `'S' → 's'` remap | reachable |
| `W8-F13-1` the upper-caser runs BEFORE the width justifier | yes | **PRESENT**, and moved: it is now inside the units-carrying general branch (`fmt_truncate_units` → `fmt_upper_case_units` → `fmt_pad_to_width`), with a `debug_assert!(!uppercase_result)` on the numeric path to keep a second copy from growing back | reachable |

Two further things the same reading settled, both of which would otherwise have
been re-nominated by this lane:

* **`F28-1`'s "an explicit `null` locale is `Locale.US`" landed.**
  `fmt_date_name` returns `None` for `FmtLocale::Given(None)` before the
  re-entrancy latch, so the caller prints the English table — which IS
  `DateFormatSymbols.getInstance(Locale.US)`. Confirmed against the oracle
  under a `de_DE` default (§3.4).
* **`W7-34`'s BLOCKING co-requisite in `lib.rs` is closed.**
  `native_printf_locale` (`lib.rs:27889`) now honours its `Locale` and calls
  `native_string_format_locale`; the `java/util/Formatter` `()V` constructor at
  `lib.rs:~21573` and `native_formatter_init` at `lib.rs:~43583` both write
  `formatter_default_locale(ctx)` into slot 1 rather than null. **`RJdkHello`
  is not at risk from this lane's changes**, and there is no locale-slot
  nomination to make. Oracle confirmation: `new Formatter().locale()` is
  `de_DE` under a `de_DE` default, `new Formatter(sb, (Locale) null).locale()`
  is `null`, and the two format `-1.234.567` and `-1,234,567` respectively —
  which is exactly the split the current code implements.

---

## 2. The four measured divergences, and what changed

### 2.1 `IllegalFormatConversionException` reports the LOWER-case conversion — 61 cells

`FormatSpecifier.conversion(char)` folds every upper-case conversion:

```java
if (Character.isUpperCase(conv)) { f.add(Flags.UPPERCASE); this.c = Character.toLowerCase(conv); }
```

and `c` is what `failConversion` hands the exception. MEASURED, `Boolean.TRUE`
as the argument, message and `getConversion()` read separately:

```
UC	%X	EX[java.util.IllegalFormatConversionException|x != java.lang.Boolean conv=x cls=java.lang.Boolean]
UC	%E	EX[java.util.IllegalFormatConversionException|e != java.lang.Boolean conv=e cls=java.lang.Boolean]
UC	%G	EX[java.util.IllegalFormatConversionException|g != java.lang.Boolean conv=g cls=java.lang.Boolean]
UC	%A	EX[java.util.IllegalFormatConversionException|a != java.lang.Boolean conv=a cls=java.lang.Boolean]
UC2	%A	EX[java.util.IllegalFormatConversionException|a != java.math.BigDecimal conv=a cls=java.math.BigDecimal]
UC2	%E	EX[java.util.IllegalFormatConversionException|e != java.lang.String conv=e cls=java.lang.String]
```

**CratonVM before (PREDICTED, from reading).** `format_arg_full` remaps only
the four conversions whose RENDERING changes — `'S'→'s'`, `'B'→'b'`,
`'C'→'c'`, `'H'→'h'` — because `'X'`/`'E'`/`'G'`/`'A'` emit upper-case digits
from `format_arg` itself. So `format_arg` receives `'X'` as written and its
applicability screen raised `FmtFault::WrongType('X', class_id)`, giving
`"X != java.lang.Boolean"` and `getConversion() == 'X'`.

**Denominator.** In the conversion × argument matrix alone this is
12 (`%X`) + 16 (`%E`) + 16 (`%G`) + 17 (`%A`) = **61 refusing cells**, every
one of them with the wrong character in the message and in the accessor. The
already-remapped `%C` was correct (`c != java.lang.Boolean`, measured) because
its remap happens for a different reason.

**Fixed** in `format_arg`'s applicability screen:
`FmtFault::WrongType(spec.to_ascii_lowercase(), class_id)`. It is done there
and *not* in `fmt_raise`, because `%t` is the opposite rule — `printDateTime`
reports the FIELD character as typed, MEASURED
`"Y != java.lang.String"` for `String.format("%tY", "x")` — and
`extract_temporal_fields` constructs its own `WrongType(field, cid)` which must
stay unfolded.

### 2.2 `%<` with no previous conversion must REFUSE

MEASURED:

```
REL	%<s alone	EX[java.util.MissingFormatArgumentException|Format specifier '%<s']
REL	%<tY alone	EX[java.util.MissingFormatArgumentException|Format specifier '%<tY']
REL	null args %<s	EX[java.util.MissingFormatArgumentException|Format specifier '%<s']
REL	null args %s	OK[null]
REL	%s%<s	OK[aa]
REL	%2$s%<s	OK[bb]
REL	%1$s%<s	OK[aa]
REL	%s%s%<s	OK[abb]
REL	%3$s%<s two args	EX[java.util.MissingFormatArgumentException|Format specifier '%3$s']
```

`Formatter.format`'s loop is `case -1 -> { if (last < 0 || (args != null &&
last > args.length - 1)) throw new MissingFormatArgumentException(fs.toString());
… }` with `last` initialised to `-1`. The `last < 0` half is **unconditional on
the varargs array**, which is why `%<s` with a null array refuses while a plain
`%s` with a null array prints `null` — the two rows above that look
inconsistent and are not.

**CratonVM before (PREDICTED, from reading).** Both the general arm and the
`'t'`/`'T'` arm of `format_impl` computed
`let use_idx = if flags.contains('<') { last_used_index.unwrap_or(0) }` —
so a format string that OPENS with `%<s` silently formatted `args[0]`.
`String.format("%<s", "a")` was `"a"`.

**Fixed** in both arms. The general arm raises
`FmtFault::MissingArgument(fmt_spec_text(…))` AFTER `fmt_check_spec`, because
in the JDK every `FormatSpecifier` constructor check runs before `format`'s
range test — so `%<0d` is still the flag mismatch and not this. The `%t` arm
raises `MissingArgument(dt_spec_text())` after `checkDateTime`'s four refusals,
which is the same ordering. Note `'<'` is already the last character of
`FMT_FLAG_ORDER`, so the quoted specifier text is `'%<s'` / `'%<tY'` without
further work.

### 2.3 `java.time.OffsetTime` had no decoder arm — 14 cells refused that must print

Sweeping **all 31 `%t` fields against 10 temporal source types** (310 cells)
found one type with no arm in `extract_temporal_fields`, so every field fell to
its `else` and refused. MEASURED, `OffsetTime` of `05:00:45.123+05:30`:

```
X	OffsetTime	H	%tH	OK[05]        X	OffsetTime	z	%tz	OK[+0530]
X	OffsetTime	I	%tI	OK[05]        X	OffsetTime	Z	%tZ	OK[+05:30]
X	OffsetTime	k	%tk	OK[5]         X	OffsetTime	R	%tR	OK[05:00]
X	OffsetTime	l	%tl	OK[5]         X	OffsetTime	T	%tT	OK[05:00:45]
X	OffsetTime	M	%tM	OK[00]        X	OffsetTime	r	%tr	OK[05:00:45 AM]
X	OffsetTime	S	%tS	OK[45]
X	OffsetTime	L	%tL	OK[123]
X	OffsetTime	N	%tN	OK[123000000]
X	OffsetTime	p	%tp	OK[am]
```

and its 17 refusals, whose characters are the ones the existing group model
already produces:

```
T	OffsetTime	s	%ts	EX[java.util.IllegalFormatConversionException|s != java.time.OffsetTime]
T	OffsetTime	Q	%tQ	EX[java.util.IllegalFormatConversionException|Q != java.time.OffsetTime]
T	OffsetTime	D	%tD	EX[java.util.IllegalFormatConversionException|m != java.time.OffsetTime]
T	OffsetTime	F	%tF	EX[java.util.IllegalFormatConversionException|F != java.time.OffsetTime]
T	OffsetTime	c	%tc	EX[java.util.IllegalFormatConversionException|a != java.time.OffsetTime]
```

**CratonVM before (PREDICTED).** `extract_temporal_fields` has arms for `Long`,
`Date`, `Calendar`, `Instant`, `LocalDate`, `LocalTime`, and the
`LocalDateTime`/`ZonedDateTime`/`OffsetDateTime` triple. `java/time/OffsetTime`
is a subclass of none of them, so all 31 fields reached
`Err(WrongType(field, cid))` — 14 of them wrongly, and the other 17 with the
wrong character on `%tD` (`D` for `m`), `%tR`/`%tT` (`R`/`T` for `H`), `%tr`
(`r` for `I`) and `%tc` (`c` for `a`).

**Fixed.** A new arm: `LocalTime`'s four field reads plus
`fmt_temporal_zone(ctx, obj)`, and
`FmtSupport { year: false, month: false, day: false, time: true,
sub_second: true, instant: false, zone: true, calendar_printer: false }`.
`%tZ` reaches `fmt_temporal_zone_name`'s non-`ZonedDateTime` arm, whose
`getOffset()` → `ZoneOffset.getId()` is exactly the measured `+05:30`. It is
NOT `instant`-capable: `INSTANT_SECONDS` needs a date the type does not carry,
which is why `%ts` and `%tQ` refuse.

### 2.4 The `date` support flag was one flag where the JDK has three — 20 more cells

The same sweep found four `java.time` types that support a PROPER SUBSET of the
date group. MEASURED (`Locale.US`), answered sets out of 31:

| source | answers | count |
|---|---|---|
| `YearMonth.of(2020,2)` | `%tY`=`2020` `%ty`=`20` `%tC`=`20` `%tm`=`02` `%tB`=`February` `%tb`=`%th`=`Feb` | 7 |
| `MonthDay.of(1,2)` | `%tm`=`01` `%td`=`02` `%te`=`2` `%tB`=`January` `%tb`=`%th`=`Jan` | 6 |
| `Year.of(2020)` | `%tY`=`2020` `%ty`=`20` `%tC`=`20` | 3 |
| `Month.JANUARY` | `%tm`=`01` `%tB`=`January` `%tb`=`%th`=`Jan` | 4 |

A single `date` flag cannot express any of those four rows — and, because the
three composites report the FIRST missing INNER field, it cannot express their
refusal characters either. MEASURED:

| source | `%tD` | `%tF` | `%tc` |
|---|---|---|---|
| `Instant` | `m` | `F` | `a` |
| `LocalTime` | `m` | `F` | `a` |
| `OffsetTime` | `m` | `F` | `a` |
| `Year` | `m` | **`m`** | `a` |
| `YearMonth` | **`d`** | **`d`** | `a` |
| `MonthDay` | **`y`** | `F` | `a` |
| `Month` | **`d`** | `F` | `a` |
| `LocalDate` | ok | ok | `H` |
| `LocalDateTime` | ok | ok | `Z` |
| `DayOfWeek` | `m` | `F` | **`b`** |

`%tD` is `mm/dd/yy` and reports `m`, then `d`, then `y`. `%tF` is
`YYYY-mm-dd` and reports its OWN `F` when the YEAR is missing (its
`ISO_STANDARD_DATE` arm reads the year with `t.get(yearField)` INLINE rather
than delegating) and `m`, then `d`, otherwise. `%tc` is
`a b d T Z Y` and reports the first missing of those.

**CratonVM before (PREDICTED).** `FmtSupport` carried one `date: bool` covering
`YEAR_OF_ERA`, `MONTH_OF_YEAR`, `DAY_OF_MONTH`, `DAY_OF_WEEK` and
`DAY_OF_YEAR`; `Year`, `YearMonth`, `MonthDay` and `Month` had no decoder arm
at all and refused all 31 fields each.

**Fixed.** `date` is now three flags — `year`, `month`, `day` — and
`fmt_temporal_fault_char` walks the composites' inner fields in order.
`DAY_OF_WEEK` (`%tA %ta`) and `DAY_OF_YEAR` (`%tj`) are DERIVED as
`year && month && day` (`FmtSupport::full_date`), which is exact for all 11
measured sources except one; see §4.1. Four new decoder arms read
`YearMonth.getYear()/getMonthValue()`, `MonthDay.getMonthValue()/
getDayOfMonth()`, `Year.getValue()` and `Month.getValue()`. Unread slots are
left at the epoch defaults and are unreachable, exactly as `Instant`'s already
are.

The unit test `temporal_support_matrix_matches_hotspot` was extended with the
five new sources, their complete answered sets, and their three composite
refusal characters. It is a PURE test over `fmt_temporal_fault_char` — no
`NativeContext` — so it runs under `cargo test -p cratonvm-native-builtins`
without a VM.

---

## 3. What the sweep found that was ALREADY RIGHT

This half matters as much as §2: it is the part of the surface that needs no
work, and recording it stops the next lane re-measuring it.

### 3.1 The conversion × argument matrix

19 conversions × 21 argument kinds = 399 cells, `Locale.US`:

| conv | prints | refuses | conv | prints | refuses |
|---|---|---|---|---|---|
| `b` `B` | 21 | 0 | `d` `o` `x` `X` | 9 | 12 |
| `h` `H` | 21 | 0 | `e` `E` `f` `g` `G` | 5 | 16 |
| `s` `S` | 21 | 0 | `a` `A` | 4 | 17 |
| `c` `C` | 5 | 16 | | | |

Every one of those matches what `lang_string.rs` computes, including the four
that are easy to get wrong and are right:

* `%b` of a zero-valued `Integer` is `true`, not `false` — the class is the
  question and `format_arg` asks it before unboxing.
* `%h` of `Long.valueOf(-42)` is `29`, not `ffffffd6`: it is
  `Long.hashCode()`, and the code invokes `hashCode` virtually rather than
  reading a slot.
* `%a` is the one float conversion that refuses a `BigDecimal`
  (`a != java.math.BigDecimal`); `%e`/`%f`/`%g` accept it.
* `%x` of `Byte.valueOf(-1)` is `ff` and of `Short.valueOf(-1)` is `ffff` —
  the 8- and 16-bit masks, which `format_arg` recovers from the wrapper class
  after `unbox_obj` has collapsed all three to `Value::Int`.

### 3.2 The flag legality surface, including the `'^'` nobody expects

13 flag sets × 19 conversions = 247 cells. The one worth transcribing is the
INTERNAL uppercase flag, which `Flags.toString` renders as `'^'` between `'-'`
and `'#'` and which `IllegalFormatFlagsException` therefore reports:

```
FLAG	X	[+ ]	%+ 8X	EX[java.util.IllegalFormatFlagsException|Flags = '^+ ']
FLAG	X	[-0]	%-08X	EX[java.util.IllegalFormatFlagsException|Flags = '-^0']
FLAG	x	[+ ]	%+ 8x	EX[java.util.IllegalFormatFlagsException|Flags = '+ ']
FLAG	x	[-0]	%-08x	EX[java.util.IllegalFormatFlagsException|Flags = '-0']
```

`fmt_check_spec`'s `all_flags` closure already inserts `'^'` at
`usize::from(s.starts_with('-'))` for an upper-case conversion, and
`fmt_spec_text` deliberately does not (because `FormatSpecifier.toString`
removes it again and re-uppercases the conversion character). Both halves are
right. `%+ 8E`, `%-08E`, `%+ 8G`, `%-08G`, `%+ 8A`, `%-08A` are the other six
cells that see it.

`checkBadFlags` ACCUMULATES rather than failing on the first bad flag, and
reports the accumulated set in `Flags.toString` order. MEASURED:
`%,08b` → `Flags = 0,`; `%#08c` → `Flags = #0`; `%+(8b` → `Flags = +(`;
`%(+012.3s` → `Flags = +0(`. `fmt_check_spec`'s `mismatch` closure filters
`fmt_flags_string(flags)` (canonical order `-#+ 0,(<`) through the bad set,
which reproduces all four.

The order-sensitive triples are right too: `%-s` is
`MissingFormatWidthException: %-s` while `%0s` is
`FormatFlagsConversionMismatchException: Conversion = s, Flags = 0` — because
only `'-'` requires a width in `checkGeneral` — and `%#s` is a mismatch while
`%#-s` is `MissingFormatWidthException: %-#s`.

### 3.3 `Formattable`, and what does NOT dispatch

```
FMTBL	%s	OK[fmtbl]        FMTBL2	%s	OK[<F f=0 w=-1 p=-1>]
FMTBL	%S	OK[fmtbl-UP]     FMTBL2	%S	OK[<F f=2 w=-1 p=-1>]
FMTBL	%-10s	OK[fmtbl-LJ]   FMTBL2	%12s	OK[<F f=0 w=12 p=-1>]
FMTBL	%#s	OK[fmtbl-ALT]    FMTBL2	%-12s	OK[<F f=1 w=12 p=-1>]
FMTBL	%#S	OK[fmtbl-UP-ALT] FMTBL2	%.4s	OK[<F f=0 w=-1 p=4>]
FMTBL	%10s	OK[fmtbl]        FMTBL2	%#s	OK[<F f=4 w=-1 p=-1>]
FMTBLC	b	%b	OK[true]
FMTBLC	h	%h	OK[18518ccf]
FMTBLC	d	%d	EX[java.util.IllegalFormatConversionException|d != FmtProbe$Fmtbl]
FLAGCONST	LEFT_JUSTIFY=1 UPPERCASE=2 ALTERNATE=4
```

`%10s` of a `Formattable` that ignores its width is `fmtbl`, five characters —
the width is NOT applied afterwards. `%b` and `%h` do NOT dispatch. Both are
what `fmt_formattable_dispatch` and its `spec == 's'` call-site guard already
do.

### 3.4 The locale slot, under a `de_DE` default

```
DEF	de_DE
NL	%,d	OK[-1,234,567]      NOARG	%,d	-1.234.567
NL	%,.2f	OK[1,234.50]      NOARG	%tB	Januar
NL	%tB	OK[January]         NOARG	%S i	I
NL	%tb	OK[Jan]
NL	%tA	OK[Thursday]
NL	%tp	OK[am]
NL	%tZ	OK[IST]
NL	%S	OK[I]
FL	new Formatter().locale()	de_DE
FL	new Formatter().format	-1.234.567
FL	new Formatter(sb,null).locale()	null
FL	new Formatter(sb,null).format	-1,234,567
```

Three different rules over one `Locale` argument, and the JDK means all three:

| consumer | explicit `null` means |
|---|---|
| `DecimalFormatSymbols` (`%,d %f %e %g`) | ROOT constants — `getGroupingSeparator(Locale)` is literally `locale == null ? ',' : …` |
| `DateFormatSymbols` (`%tB %tb %tA %ta %tp`) and the `%tZ` display name | **`Locale.US`** — `printDateTime`'s `Locale lt = Objects.requireNonNullElse(l, Locale.US)` |
| `toUpperCaseWithLocale` (`%S %C %B %H`, `%T`) | the DEFAULT — `Objects.requireNonNullElse(l, Locale.getDefault(FORMAT))` |

`fmt_symbols_for`, `fmt_date_name` and `fmt_upper_case` implement exactly these
three. The `%S` of `"i"` row cannot discriminate under `de_DE`; `F28-1` already
measured it as U+0130 under a `tr_TR` default, which is the DEFAULT rule.

### 3.5 Numeric localization

10 locales × 16 cells. The rules the file implements and the oracle confirms:
the zero digit and the two separators are substituted for `%d %f %e %g`; `%x`,
`%o`, `%a` and `%s` are NOT localized; the exponent's DIGITS are localized but
its `e`/`E` and sign are not (`ar-EG` `%e` of `-1234.5` is
`-\u0661\u066b\u0662\u0663\u0664\u0665\u0660\u0660e+\u0660\u0663`);
`NaN`/`Infinity` are never localized; the grouping separator under `fr-FR` is
U+202F NARROW NO-BREAK SPACE; and the leading minus of every negative number is
ASCII U+002D even under a locale whose `getMinusSign()` is U+2212 — the ONE
localized minus in the whole class is `%tF`'s year sign
(`lt-LT` `%tF` of `-44-03-15` is `\u22120044-03-15`, measured), which
`fmt_iso_year` already handles.

### 3.6 The parser

Every one of these matches `format_impl` as read:

```
IDX	%2147483648s	EX[java.util.IllegalFormatWidthException|-2147483648]
IDX	%.2147483648s	EX[java.util.IllegalFormatPrecisionException|-2147483648]
IDX	%0$s	EX[java.util.IllegalFormatArgumentIndexException|Illegal format argument index = 0]
IDX	%--s	EX[java.util.DuplicateFormatFlagsException|Flags = '-']
IDX	%	EX[java.util.UnknownFormatConversionException|Conversion = '%']
IDX	%q	EX[java.util.UnknownFormatConversionException|Conversion = 'q']
TP	%t%	EX[java.util.UnknownFormatConversionException|Conversion = 't%']
TP	%t1	EX[java.util.UnknownFormatConversionException|Conversion = 't']
TP	%tt	EX[java.util.UnknownFormatConversionException|Conversion = 'tt']
TP	%TT	OK[05:30:00]
TERR	%tQQ	OK[1755300645123Q]
PCTW	%.2%	EX[java.util.IllegalFormatPrecisionException|2]
PCTW	%+%	EX[java.util.IllegalFormatFlagsException|Flags = '+']
NLW	%5n	EX[java.util.IllegalFormatWidthException|5]
NLW	%.2n	EX[java.util.IllegalFormatPrecisionException|2]
NLW	%-n	EX[java.util.IllegalFormatFlagsException|Flags = '-']
```

### 3.7 The null argument under every conversion

19 conversions × 5 decorations = 95 cells, all matching the shared-printer
branch: `%X` of null is `NULL`, `%010d` of null is `      null` (SPACE-padded,
because zero padding lives in a printer a null never enters), `%.2f` of null is
`nu`, `%.2d` of null is still `IllegalFormatPrecisionException`, and `%010s` of
null is still the flag mismatch.

---

## 4. Residuals — MEASURED, deliberately NOT fixed

### 4.1 `java.time.DayOfWeek`

MEASURED: `%tA` of `DayOfWeek.MONDAY` is `Monday` and `%ta` is `Mon`; the other
29 fields refuse, and `%tc` reports **`b`**, not `a`. It is the one measured
source that supports `DAY_OF_WEEK` with no year, month or day — so
`FmtSupport::full_date` cannot express it, and neither can
`format_temporal_field`, whose weekday comes from `day_of_week_sun0(year,
month, day)`. Fixing it needs a day-of-week that does not come from a date,
which is a signature change through `extract_temporal_fields` →
`format_temporal_field`. Left refused: `%tA` of a `DayOfWeek` is
`A != java.time.DayOfWeek` on CratonVM (PREDICTED) where HotSpot prints
`Monday`. **2 cells.**

### 4.2 The four non-ISO `ChronoLocalDate`s

MEASURED, `HijrahDate`, `JapaneseDate`, `ThaiBuddhistDate` and `MinguoDate` all
answer the same 14 fields (`B b h A a C Y y j m d e D F`) and refuse 17, with
`%tR`/`%tT` → `H`, `%tr` → `I` and `%tc` → `H`. CratonVM refuses all 31, and 17
of those refusals already carry the right character while 4 do not (`R`, `T`,
`r`, `c`). Supporting them means deciding what `%tY` of a `JapaneseDate` is —
an era-relative year in a calendar system this VM does not model — and that is
a `java.time.chrono` question, not a `Formatter` one. Left refused.
**56 cells across four types.**

### 4.3 `%<` and the streaming parser

`java.util.Formatter` parses the WHOLE format string into `FormatSpecifier`
objects before printing anything, so `"%s %q"` throws with no output;
`format_impl` is a streaming parser that also throws before returning a string,
so the two agree at the `String.format` boundary. They would NOT agree for a
`Formatter` writing into a caller-visible `Appendable`, where HotSpot appends
nothing and CratonVM would have appended the first conversion. Not measured on
the `Appendable` route; noted so the next lane knows the shape.

### 4.4 What was not swept

Precision above 30, widths above 2^31, `%s` of an object whose `toString()`
throws, `Formatter` re-entrancy through a `Formattable` that formats itself,
and the `%t` name fields under every one of the JDK's 1,158 locales (11 were
swept). None of these is known to diverge; none was measured.

---

## 5. NOMINATIONS

**None.** This is deliberate and is the result of a check, not of not looking:

* The `lib.rs` locale-slot residual that `W7-34` and `F28-1` both nominated is
  **already applied** — see §1. Re-nominating it would have been a
  prediction dressed as a finding.
* `native_formatter_format` (`lib.rs:~43655`) and the two
  `java/util/Formatter` registrations (`lib.rs:~21573`, `~43447`) were read and
  need no change for anything in §2: all four defects are inside
  `format_impl` / `format_arg` / `extract_temporal_fields`, which both
  registrars reach through `lang_string`.
* No change is needed in `regression-suite/` beyond the vector in §6, which is
  written out here rather than created because another lane owns that
  directory this wave.

---

## 6. The regression vector this behaviour wants

Not created — `regression-suite/src/` belongs to another lane this wave. This
is the source to add as `regression-suite/src/RJdkFormatSurface.java`, and it
needs registering in `CORE_CLASSES`. Every expectation below is a MEASURED
oracle value from §2. **Labels are ASCII only** — HANDOFF §7's em-dash row.

```java
import java.time.*;
import java.util.*;

/** G2-1: the java.util.Formatter conversion surface. All expectations
 *  measured on HotSpot 25.0.3+9-LTS, 2026-08-16, Locale.US. */
public class RJdkFormatSurface {

    static int checks = 0, fails = 0;

    static void eq(String label, String expect, String actual) {
        checks++;
        boolean ok = expect.equals(actual);
        if (!ok) fails++;
        System.out.println("CK " + (ok ? "PASS" : "FAIL") + " " + label
                + " expect=[" + expect + "] actual=[" + actual + "]");
    }

    static void throwsWith(String label, String expectClass, String expectMsg,
                           java.util.function.Supplier<String> body) {
        checks++;
        String got;
        try {
            got = "no-throw:" + body.get();
        } catch (Throwable t) {
            got = t.getClass().getName() + "|" + t.getMessage();
        }
        String want = expectClass + "|" + expectMsg;
        boolean ok = want.equals(got);
        if (!ok) fails++;
        System.out.println("CK " + (ok ? "PASS" : "FAIL") + " " + label
                + " expect=[" + want + "] actual=[" + got + "]");
    }

    public static void main(String[] args) {
        final Locale US = Locale.US;

        // 2.1 the exception carries the LOWER-case conversion.
        throwsWith("upperConvX", "java.util.IllegalFormatConversionException",
                "x != java.lang.Boolean", () -> String.format(US, "%X", Boolean.TRUE));
        throwsWith("upperConvE", "java.util.IllegalFormatConversionException",
                "e != java.lang.String", () -> String.format(US, "%E", "s"));
        throwsWith("upperConvG", "java.util.IllegalFormatConversionException",
                "g != java.lang.Boolean", () -> String.format(US, "%G", Boolean.TRUE));
        throwsWith("upperConvA", "java.util.IllegalFormatConversionException",
                "a != java.math.BigDecimal",
                () -> String.format(US, "%A", new java.math.BigDecimal("1.5")));
        // ...and %t does NOT fold.
        throwsWith("dateConvNotFolded", "java.util.IllegalFormatConversionException",
                "Y != java.lang.String", () -> String.format(US, "%tY", "x"));
        // getConversion() carries it too, not only the message.
        checks++;
        try {
            String.format(US, "%X", Boolean.TRUE);
            fails++;
            System.out.println("CK FAIL upperConvAccessor no-throw");
        } catch (IllegalFormatConversionException e) {
            boolean ok = e.getConversion() == 'x';
            if (!ok) fails++;
            System.out.println("CK " + (ok ? "PASS" : "FAIL")
                    + " upperConvAccessor expect=[x] actual=[" + e.getConversion() + "]");
        }

        // 2.2 relative index with no previous conversion.
        throwsWith("relNoPrevious", "java.util.MissingFormatArgumentException",
                "Format specifier '%<s'", () -> String.format(US, "%<s", "a"));
        throwsWith("relNoPreviousDate", "java.util.MissingFormatArgumentException",
                "Format specifier '%<tY'", () -> String.format(US, "%<tY", 0L));
        eq("relAfterOrdinary", "aa", String.format(US, "%s%<s", "a"));
        eq("relAfterExplicit", "bb", String.format(US, "%2$s%<s", "a", "b"));
        eq("relSecondOrdinary", "abb", String.format(US, "%s%s%<s", "a", "b"));
        // the check runs after the parse-time flag checks.
        throwsWith("relLosesToFlagCheck", "java.util.FormatFlagsConversionMismatchException",
                "Conversion = s, Flags = 0", () -> String.format(US, "%<0s", "a"));

        // 2.3 OffsetTime.
        OffsetTime ot = Instant.ofEpochMilli(1755300645123L)
                .atZone(ZoneId.of("Asia/Kolkata")).toOffsetDateTime().toOffsetTime();
        eq("offsetTimeH", "05", String.format(US, "%tH", ot));
        eq("offsetTimeT", "05:00:45", String.format(US, "%tT", ot));
        eq("offsetTimeR", "05:00:45 AM", String.format(US, "%tr", ot));
        eq("offsetTimeN", "123000000", String.format(US, "%tN", ot));
        eq("offsetTimeSmallZ", "+0530", String.format(US, "%tz", ot));
        eq("offsetTimeBigZ", "+05:30", String.format(US, "%tZ", ot));
        eq("offsetTimeP", "am", String.format(US, "%tp", ot));
        throwsWith("offsetTimeNoInstant", "java.util.IllegalFormatConversionException",
                "s != java.time.OffsetTime", () -> String.format(US, "%ts", ot));
        throwsWith("offsetTimeNoDate", "java.util.IllegalFormatConversionException",
                "Y != java.time.OffsetTime", () -> String.format(US, "%tY", ot));
        throwsWith("offsetTimeDComposite", "java.util.IllegalFormatConversionException",
                "m != java.time.OffsetTime", () -> String.format(US, "%tD", ot));
        throwsWith("offsetTimeCComposite", "java.util.IllegalFormatConversionException",
                "a != java.time.OffsetTime", () -> String.format(US, "%tc", ot));

        // 2.4 the partial java.time types.
        YearMonth ym = YearMonth.of(2020, 2);
        MonthDay md = MonthDay.of(1, 2);
        eq("yearMonthY", "2020", String.format(US, "%tY", ym));
        eq("yearMonthM", "02", String.format(US, "%tm", ym));
        eq("yearMonthB", "February", String.format(US, "%tB", ym));
        throwsWith("yearMonthNoDay", "java.util.IllegalFormatConversionException",
                "d != java.time.YearMonth", () -> String.format(US, "%td", ym));
        throwsWith("yearMonthFComposite", "java.util.IllegalFormatConversionException",
                "d != java.time.YearMonth", () -> String.format(US, "%tF", ym));
        eq("monthDayD", "02", String.format(US, "%td", md));
        eq("monthDayE", "2", String.format(US, "%te", md));
        eq("monthDayB", "January", String.format(US, "%tB", md));
        throwsWith("monthDayNoYear", "java.util.IllegalFormatConversionException",
                "Y != java.time.MonthDay", () -> String.format(US, "%tY", md));
        throwsWith("monthDayDComposite", "java.util.IllegalFormatConversionException",
                "y != java.time.MonthDay", () -> String.format(US, "%tD", md));
        throwsWith("monthDayFComposite", "java.util.IllegalFormatConversionException",
                "F != java.time.MonthDay", () -> String.format(US, "%tF", md));
        eq("yearY", "2020", String.format(US, "%tY", Year.of(2020)));
        eq("yearC", "20", String.format(US, "%tC", Year.of(2020)));
        throwsWith("yearFComposite", "java.util.IllegalFormatConversionException",
                "m != java.time.Year", () -> String.format(US, "%tF", Year.of(2020)));
        eq("monthM", "01", String.format(US, "%tm", Month.JANUARY));
        eq("monthB", "January", String.format(US, "%tB", Month.JANUARY));
        throwsWith("monthDComposite", "java.util.IllegalFormatConversionException",
                "d != java.time.Month", () -> String.format(US, "%tD", Month.JANUARY));

        // 3.2 the internal uppercase flag, which Flags.toString renders as ^.
        throwsWith("upperFlagCaret", "java.util.IllegalFormatFlagsException",
                "Flags = '^+ '", () -> String.format(US, "%+ 8X", -42));
        throwsWith("upperFlagCaretDash", "java.util.IllegalFormatFlagsException",
                "Flags = '-^0'", () -> String.format(US, "%-08E", -1.5d));
        throwsWith("lowerFlagNoCaret", "java.util.IllegalFormatFlagsException",
                "Flags = '+ '", () -> String.format(US, "%+ 8x", -42));

        System.out.println("checks=" + checks + " fails=" + fails);
        if (fails != 0) throw new AssertionError(fails + " check(s) failed");
    }
}
```

---

## 7. The probes

Run as `"$JAVA_HOME/bin/java" -Dfile.encoding=UTF-8 <Probe>.java` under
JDK 25 single-file source mode. `FmtProbe` was run with
`-Duser.language=en -Duser.country=US`; `TProbe` with the same plus
`-Duser.timezone=Asia/Kolkata`; `T2Probe` with
`-Duser.language=de -Duser.country=DE -Duser.timezone=Asia/Kolkata`, because
the null-locale rows in §3.4 are only discriminating under a non-US default.

Every probe prints non-ASCII as `\uXXXX` through the same `show()` helper, so
the transcript is pure ASCII and cannot be mangled by the Windows console code
page — HANDOFF §7's trap, applied to the probe rather than to a check label.

### 7.1 `FmtProbe.java` — the conversion surface (1,059 cells)

```java
import java.util.*;
import java.math.*;
import java.util.Formattable;
import java.util.Formatter;
import java.util.FormattableFlags;

public class FmtProbe {

    static String show(String s) {
        if (s == null) return "<null-ref>";
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '\n') b.append("\\n");
            else if (c == '\r') b.append("\\r");
            else if (c == '\t') b.append("\\t");
            else if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }

    static void cell(String tag, String fmt, Object arg) { cell(tag, Locale.US, fmt, arg); }

    static void cell(String tag, Locale loc, String fmt, Object arg) {
        String res;
        try {
            res = "OK[" + show(String.format(loc, fmt, arg)) + "]";
        } catch (Throwable t) {
            String m = t.getMessage();
            res = "EX[" + t.getClass().getName() + "|" + (m == null ? "<null-msg>" : show(m)) + "]";
        }
        System.out.println(tag + "\t" + fmt + "\t" + res);
    }

    static class Bean {
        public String toString() { return "BEAN"; }
        public int hashCode() { return 0xDEADBEEF; }
    }

    static class Fmtbl implements Formattable {
        private final String name;
        Fmtbl(String n) { name = n; }
        public void formatTo(Formatter f, int flags, int width, int precision) {
            f.format("<%s f=%d w=%d p=%d>", name, flags, width, precision);
        }
        public String toString() { return "FMTBL-toString"; }
    }

    static class FmtblPlain implements Formattable {
        public void formatTo(Formatter f, int flags, int width, int precision) {
            StringBuilder sb = new StringBuilder("fmtbl");
            if ((flags & FormattableFlags.UPPERCASE) != 0) sb.append("-UP");
            if ((flags & FormattableFlags.LEFT_JUSTIFY) != 0) sb.append("-LJ");
            if ((flags & FormattableFlags.ALTERNATE) != 0) sb.append("-ALT");
            f.format("%s", sb.toString());
        }
    }

    static final Object[] ARGS;
    static final String[] ARGNAMES;
    static {
        List<Object> a = new ArrayList<Object>();
        List<String> n = new ArrayList<String>();
        a.add(null);                        n.add("null");
        a.add(Boolean.TRUE);                n.add("Boolean.TRUE");
        a.add(Boolean.FALSE);               n.add("Boolean.FALSE");
        a.add(Byte.valueOf((byte) 42));     n.add("Byte(42)");
        a.add(Short.valueOf((short) 42));   n.add("Short(42)");
        a.add(Character.valueOf('Z'));      n.add("Character(Z)");
        a.add(Integer.valueOf(42));         n.add("Integer(42)");
        a.add(Integer.valueOf(-42));        n.add("Integer(-42)");
        a.add(Long.valueOf(42L));           n.add("Long(42)");
        a.add(Long.valueOf(-42L));          n.add("Long(-42)");
        a.add(Float.valueOf(1.5f));         n.add("Float(1.5)");
        a.add(Double.valueOf(1.5d));        n.add("Double(1.5)");
        a.add(Double.valueOf(-1.5d));       n.add("Double(-1.5)");
        a.add(new BigInteger("42"));        n.add("BigInteger(42)");
        a.add(new BigInteger("-42"));       n.add("BigInteger(-42)");
        a.add(new BigDecimal("1.5"));       n.add("BigDecimal(1.5)");
        a.add("abc");                       n.add("String(abc)");
        a.add("");                          n.add("String(empty)");
        a.add(new Bean());                  n.add("Bean");
        a.add(new Fmtbl("F"));              n.add("Formattable");
        a.add(new int[]{1,2});              n.add("int[]");
        ARGS = a.toArray();
        ARGNAMES = n.toArray(new String[0]);
    }

    static final char[] CONVS = {'b','B','h','H','s','S','c','C','d','o','x','X','e','E','f','g','G','a','A'};

    static void section(String s) { System.out.println(); System.out.println("##### " + s); }

    public static void main(String[] args) throws Exception {
        System.out.println("JAVA " + System.getProperty("java.version") + " vendor=" + System.getProperty("java.vendor"));
        System.out.println("default locale = " + Locale.getDefault());
        System.out.println("default tz = " + TimeZone.getDefault().getID());

        section("1. CONVERSION x ARGUMENT (Locale.US, plain conversion)");
        for (char c : CONVS)
            for (int i = 0; i < ARGS.length; i++)
                cell("CONV\t" + c + "\t" + ARGNAMES[i], "%" + c, ARGS[i]);

        section("2. percent and newline conversions");
        cell("PCT", "%%", null);
        cell("NL", "%n", null);
        cell("PCTW", "%5%", null);
        cell("PCTW", "%-5%", null);
        cell("PCTW", "%.2%", null);
        cell("PCTW", "%5.2%", null);
        cell("PCTW", "%+%", null);
        cell("NLW", "%5n", null);
        cell("NLW", "%.2n", null);
        cell("NLW", "%-n", null);

        section("3. FLAGS x CONVERSION width 8");
        String[] flags = {"", "-", "+", " ", "0", ",", "(", "#", "+ ", "-0", "+(", ",0", "#0"};
        for (char c : CONVS) {
            Object arg;
            switch (c) {
                case 'd': case 'o': case 'x': case 'X': arg = Integer.valueOf(-42); break;
                case 'e': case 'E': case 'f': case 'g': case 'G': case 'a': case 'A': arg = Double.valueOf(-1234.5678d); break;
                case 'c': case 'C': arg = Character.valueOf('Z'); break;
                default: arg = "abc";
            }
            for (String f : flags) cell("FLAG\t" + c + "\t[" + f + "]", "%" + f + "8" + c, arg);
        }

        section("3b. FLAGS with no width");
        for (String f : new String[]{"-","0"}) {
            cell("FLAGNOW\td\t[" + f + "]", "%" + f + "d", Integer.valueOf(-42));
            cell("FLAGNOW\ts\t[" + f + "]", "%" + f + "s", "abc");
        }

        section("4. PRECISION");
        cell("PREC", "%.2s", "abcdef");   cell("PREC", "%.0s", "abcdef");
        cell("PREC", "%.2b", Boolean.TRUE); cell("PREC", "%.2b", null);
        cell("PREC", "%.2h", "abc");      cell("PREC", "%.2s", (Object) null);
        cell("PREC", "%.2d", Integer.valueOf(42));
        cell("PREC", "%.2c", Character.valueOf('Z'));
        cell("PREC", "%.2x", Integer.valueOf(42));
        cell("PREC", "%.0f", Double.valueOf(1.5d));
        cell("PREC", "%.0f", Double.valueOf(2.5d));
        cell("PREC", "%.3e", Double.valueOf(0.0d));
        cell("PREC", "%.3a", Double.valueOf(1.5d));
        cell("PREC", "%.30f", Double.valueOf(0.1d));
        cell("PREC", "%.2g", Double.valueOf(0.00001234d));
        cell("PREC", "%.0g", Double.valueOf(0.00001234d));
        cell("PREC", "%g", Double.valueOf(0.00001234d));
        cell("PREC", "%g", Double.valueOf(123456789.0d));

        section("5. SPECIAL FLOAT VALUES");
        double[] sp = {Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, 0.0d, -0.0d};
        String[] spn = {"NaN","PosInf","NegInf","0.0","neg0.0"};
        for (char c : new char[]{'e','E','f','g','G','a','A','s'})
            for (int i = 0; i < sp.length; i++) {
                cell("SPFLT\t" + c + "\t" + spn[i], "%" + c, Double.valueOf(sp[i]));
                cell("SPFLTW\t" + c + "\t" + spn[i], "%(+012.3" + c, Double.valueOf(sp[i]));
            }

        section("6. NEGATIVE / ALTERNATE INTEGER");
        Object[] negs = {Integer.valueOf(-1), Long.valueOf(-1L), Byte.valueOf((byte)-1),
                         Short.valueOf((short)-1), new BigInteger("-1")};
        for (Object o : negs)
            for (char c : new char[]{'d','o','x','X'}) {
                cell("NEGINT\t" + o.getClass().getSimpleName() + "\t" + c, "%" + c, o);
                cell("NEGINTA\t" + o.getClass().getSimpleName() + "\t" + c, "%#016" + c, o);
                cell("NEGINTB\t" + o.getClass().getSimpleName() + "\t" + c, "%(,+016" + c, o);
            }

        section("7. PARSER / INDEX errors");
        cell("IDX", "%1$s %1$s", "a");   cell("IDX", "%2$s", "a");
        cell("IDX", "%0$s", "a");        cell("IDX", "%s %s", "a");
        cell("IDX", "%<s", "a");         cell("IDX", "%q", "a");
        cell("IDX", "%", "a");           cell("IDX", "% s", "a");
        cell("IDX", "%-s", "a");         cell("IDX", "%,s", "a");
        cell("IDX", "%#s", "a");         cell("IDX", "%#b", "a");
        cell("IDX", "%#d", Integer.valueOf(1));
        cell("IDX", "%,x", Integer.valueOf(1));
        cell("IDX", "%(s", "a");         cell("IDX", "%--s", "a");
        cell("IDX", "%2147483648s", "a");
        cell("IDX", "%.2147483648s", "a");
        cell("IDX", "%+ d", Integer.valueOf(1));
        cell("IDX", "%-0d", Integer.valueOf(1));
        cell("IDX", "%(d", Integer.valueOf(1));
        cell("IDX", "%(o", Integer.valueOf(1));
        cell("IDX", "%(x", Integer.valueOf(1));
        cell("IDX", "%+c", Character.valueOf('a'));
        cell("IDX", "%.1n", null);

        section("8. WIDTH edge");
        cell("W", "%1s", "abc");   cell("W", "%10s", "abc");
        cell("W", "%-10s|", "abc"); cell("W", "%010d", Integer.valueOf(-42));
        cell("W", "%010s", "abc");  cell("W", "%0s", "abc");
        cell("W", "%1$-10.2s|", "abcdef");

        section("9. FORMATTABLE dispatch");
        Object fb = new FmtblPlain();
        for (String f : new String[]{"%s","%S","%10s","%-10s","%.3s","%#s","%#S","%10.3s","%-10.3S","%-10.3s"})
            cell("FMTBL", f, fb);
        Object fb2 = new Fmtbl("F");
        for (String f : new String[]{"%s","%S","%12s","%-12s","%.4s","%#s"}) cell("FMTBL2", f, fb2);
        for (char c : new char[]{'b','B','h','H','d','x','f','c'}) cell("FMTBLC\t" + c, "%" + c, fb2);
        System.out.println("FLAGCONST\tLEFT_JUSTIFY=" + FormattableFlags.LEFT_JUSTIFY
                + " UPPERCASE=" + FormattableFlags.UPPERCASE
                + " ALTERNATE=" + FormattableFlags.ALTERNATE);

        section("10. hash conversions");
        for (int i = 0; i < ARGS.length; i++) {
            if (ARGS[i] == null) { cell("HASH\tnull", "%h", ARGS[i]); cell("HASHU\tnull", "%H", ARGS[i]); continue; }
            System.out.println("HASHREF\t" + ARGNAMES[i] + "\thashCode=" + Integer.toHexString(ARGS[i].hashCode()));
            cell("HASH\t" + ARGNAMES[i], "%h", ARGS[i]);
            cell("HASHU\t" + ARGNAMES[i], "%H", ARGS[i]);
        }

        section("11. boolean semantics");
        for (int i = 0; i < ARGS.length; i++) cell("BOOL\t" + ARGNAMES[i], "%b", ARGS[i]);

        section("12. char code points");
        Object[] chrs = {Character.valueOf('Z'), Byte.valueOf((byte)65), Short.valueOf((short)65),
                Integer.valueOf(65), Integer.valueOf(0x1F600), Integer.valueOf(-1), Integer.valueOf(0x110000),
                Integer.valueOf(0xD800), Long.valueOf(65L), "a", Double.valueOf(65.0)};
        for (Object o : chrs) cell("CHR\t" + o.getClass().getSimpleName() + "(" + o + ")", "%c", o);
        cell("CHR\tnull", "%c", (Object) null);
        cell("CHRU\tnull", "%C", (Object) null);

        section("13. NULL under every conversion plus flags");
        for (char c : CONVS) {
            cell("NULLC\t" + c, "%" + c, (Object) null);
            cell("NULLCW\t" + c, "%10" + c, (Object) null);
            cell("NULLCL\t" + c, "%-10" + c, (Object) null);
            cell("NULLCP\t" + c, "%.2" + c, (Object) null);
            cell("NULLC0\t" + c, "%010" + c, (Object) null);
        }

        section("14. UPPERCASE conversions on non-ASCII");
        cell("UP", "%S", "stra\u00dfe");
        cell("UP", "%S", "i");
        cell("UP", Locale.forLanguageTag("tr-TR"), "%S", "i");
        cell("UP", "%10S", "abc");   cell("UP", "%-10S", "abc");
        cell("UP", "%.2S", "abcdef"); cell("UP", "%10.2S", "abcdef");
        cell("UP", "%H", "abc");     cell("UP", "%B", Boolean.TRUE);
        cell("UP", "%C", Character.valueOf('z'));
        cell("UP", "%X", Integer.valueOf(255));
        cell("UP", "%E", Double.valueOf(1.5));
        cell("UP", "%A", Double.valueOf(1.5));
        cell("UP", "%G", Double.valueOf(0.00001234));
    }
}
```

Selected output (the full 1,113-line transcript is reproduced by re-running
the probe; every value quoted in §2 and §3 is verbatim from it):

```
JAVA 25.0.3 vendor=Eclipse Adoptium
default locale = en_US

CONV	h	null	%h	OK[null]
CONV	h	Boolean.TRUE	%h	OK[4cf]
CONV	h	Long(-42)	%h	OK[29]
CONV	h	String(abc)	%h	OK[17862]
CONV	h	Bean	%h	OK[deadbeef]
CONV	H	null	%H	OK[NULL]
CONV	X	Boolean.TRUE	%X	EX[java.util.IllegalFormatConversionException|x != java.lang.Boolean]
CONV	E	Byte(42)	%E	EX[java.util.IllegalFormatConversionException|e != java.lang.Byte]
CONV	G	Long(42)	%G	EX[java.util.IllegalFormatConversionException|g != java.lang.Long]
CONV	A	BigDecimal(1.5)	%A	EX[java.util.IllegalFormatConversionException|a != java.math.BigDecimal]
CONV	C	Boolean.TRUE	%C	EX[java.util.IllegalFormatConversionException|c != java.lang.Boolean]
CONV	c	Integer(-42)	%c	EX[java.util.IllegalFormatCodePointException|Code point = 0xffffffd6]
CONV	s	Formattable	%s	OK[<F f=0 w=-1 p=-1>]
CONV	S	Formattable	%S	OK[<F f=2 w=-1 p=-1>]
CONV	x	BigInteger(-42)	%x	OK[-2a]
CONV	o	Integer(-42)	%o	OK[37777777726]

FLAG	X	[+ ]	%+ 8X	EX[java.util.IllegalFormatFlagsException|Flags = '^+ ']
FLAG	X	[-0]	%-08X	EX[java.util.IllegalFormatFlagsException|Flags = '-^0']
FLAG	b	[,0]	%,08b	EX[java.util.FormatFlagsConversionMismatchException|Conversion = b, Flags = 0,]
FLAG	c	[#0]	%#08c	EX[java.util.FormatFlagsConversionMismatchException|Conversion = c, Flags = #0]
FLAG	s	[#0]	%#08s	EX[java.util.FormatFlagsConversionMismatchException|Conversion = s, Flags = 0]
FLAGNOW	d	[-]	%-d	EX[java.util.MissingFormatWidthException|%-d]
FLAGNOW	s	[0]	%0s	EX[java.util.FormatFlagsConversionMismatchException|Conversion = s, Flags = 0]

NEGINTA	BigInteger	x	%#016x	OK[-0x0000000000001]
NEGINTB	Integer	d	%(,+016d	OK[(00000000000001)]
SPFLTW	e	neg0.0	%(+012.3e	OK[(00.000e+00)]
SPFLTW	s	NaN	%(+012.3s	EX[java.util.FormatFlagsConversionMismatchException|Conversion = s, Flags = +0(]

IDX	%<s	EX[java.util.MissingFormatArgumentException|Format specifier '%<s']
IDX	%2147483648s	EX[java.util.IllegalFormatWidthException|-2147483648]
IDX	%0$s	EX[java.util.IllegalFormatArgumentIndexException|Illegal format argument index = 0]

NULLC0	d	%010d	OK[      null]
NULLCP	f	%.2f	OK[nu]
NULLC	X	%X	OK[NULL]
UP	%S	OK[STRASSE]
```

### 7.2 `TProbe.java` — the `%t` suffix x source matrix (1,445 cells)

The whole conversion is `String.format(loc, "%t" + c, src)` over
`SUFFIX` x `srcs`, plus a locale sweep. `SUFFIX` is every letter the spec lists
PLUS 21 letters that must be rejected, so the 31/52 split is measured rather
than assumed:

```java
static final char[] SUFFIX = {
    'H','I','k','l','M','S','L','N','p','z','Z','s','Q',
    'B','b','h','A','a','C','Y','y','j','m','d','e',
    'R','T','r','D','F','c',
    'g','G','i','J','K','n','O','P','q','t','u','v','w','x','X','f','o','U','V','W','E'
};

long epochMillis = 1755300645123L;
Date date = new Date(epochMillis);
Calendar cal = Calendar.getInstance(); cal.setTimeInMillis(epochMillis);
Instant instant = Instant.ofEpochMilli(epochMillis);
ZoneId zone = ZoneId.of("Asia/Kolkata");
ZonedDateTime zdt = instant.atZone(zone);
Object[] srcs = { Long.valueOf(epochMillis), date, cal, instant,
                  zdt.toLocalDate(), zdt.toLocalTime(), zdt.toLocalDateTime(),
                  zdt, zdt.toOffsetDateTime(), zdt.toOffsetDateTime().toOffsetTime() };
for (int i = 0; i < srcs.length; i++)
    for (char c : SUFFIX)
        cell("T\t" + names[i] + "\t" + c, Locale.US, "%t" + c, srcs[i]);
```

with further sections for `%T`, non-temporal arguments, `%t` flags/width/
precision, the `%t` parser edges, an 11-locale name sweep, a five-year sweep
(45 BC, 1 BC, AD 5, AD 999, AD 12345) x four locales, a 10-locale numeric
sweep, the explicit-null-locale rows, and `Formatter.locale()`.

Result — answered/refused out of 83 suffixes (52 of which are invalid letters
and are `UnknownFormatConversionException` for every source):

```
Long           OK 31  EX 21     Instant        OK  4  EX 48
Date           OK 31  EX 21     LocalDate      OK 14  EX 38
Calendar       OK 31  EX 21     LocalTime      OK 12  EX 40
ZonedDateTime  OK 31  EX 21     LocalDateTime  OK 26  EX 26
OffsetDateTime OK 31  EX 21     OffsetTime     OK 14  EX 38
```

which is F28-1's six-source table plus `OffsetTime`'s new row: 14 answered, 17
refused out of 31. Across the whole `%t` probe, 404 of 1,445 cells throw.

### 7.3 `T2Probe.java` — the exotic sources, the relative index, and the locale slot (428 cells)

```java
Object[] srcs = {
    zdt.toOffsetDateTime().toOffsetTime(), Year.of(2020), YearMonth.of(2020, 2),
    MonthDay.of(1, 2), Month.JANUARY, DayOfWeek.MONDAY,
    zdt.toInstant().atOffset(java.time.ZoneOffset.ofHours(3)),
    java.time.chrono.HijrahDate.now(), java.time.chrono.JapaneseDate.of(2020, 2, 3),
    java.time.chrono.ThaiBuddhistDate.of(2563, 2, 3), java.time.chrono.MinguoDate.of(109, 2, 3),
};
for (int i = 0; i < srcs.length; i++)
    for (char c : FIELDS)            // the 31 valid fields only
        cell("X\t" + names[i] + "\t" + c, Locale.US, "%t" + c, srcs[i]);
```

plus `%<` selection (`%<s` alone, `%s%<s`, `%2$s%<s`, `%1$s%<s`, `%s%s%<s`,
`%3$s%<s`, each also with a null varargs array), the `%t` parser edges
(`%t%`, `%T%`, `%t1`, `%tt`, `%TT`, `%t`, `%<tY`, `%1$tY`), the uppercase
conversion-character rows (reading `getConversion()` as well as the message),
the explicit-null-locale rows under a `de_DE` default, the `Formatter.locale()`
rows, and `%tZ`/`%tz` across five zone-bearing sources x four locales plus a
DST pair.

Answered sets, measured:

```
OffsetTime         OK(14): H I k l M S L N p z Z R T r
YearMonth          OK(7):  B b h C Y y m
MonthDay           OK(6):  B b h m d e
Year               OK(3):  C Y y
Month              OK(4):  B b h m
DayOfWeek          OK(2):  A a
HijrahDate         OK(14): B b h A a C Y y j m d e D F
JapaneseDate       OK(14): B b h A a C Y y j m d e D F
ThaiBuddhistDate   OK(14): B b h A a C Y y j m d e D F
MinguoDate         OK(14): B b h A a C Y y j m d e D F
OffsetDateTime+03  OK(31): all
```

and the refusal characters, as `<field>-><reported>`:

```
Year       D->m F->m c->a   R->H T->H r->I   (28 refused)
YearMonth  D->d F->d c->a   R->H T->H r->I   (24 refused)
MonthDay   D->y F->F c->a   R->H T->H r->I   (25 refused)
Month      D->d F->F c->a   R->H T->H r->I   (27 refused)
DayOfWeek  D->m F->F c->b   R->H T->H r->I   (29 refused)
OffsetTime D->m F->F c->a                    (17 refused)
Hijrah/Japanese/ThaiBuddhist/Minguo  c->H  R->H T->H r->I  (17 refused each)
```

---

## 8. What the orchestrator must check at build time

1. **`cargo test -p cratonvm-native-builtins temporal_support_matrix_matches_hotspot`.**
   It is pure and needs no VM. It now pins 31 fields x 5 new sources plus the
   composite refusal characters; a wrong `year`/`month`/`day` split fails it
   at the named source and field.
2. **`--dump-native-registry` for `java/util/Formatter.format`.** §1 asserts
   registrar 1 (`register_string_format_real_jdk_natives`) is the one that
   owns the slot under `--jdk-only`, reading W7-34's table rather than the
   binary. `owns_slot=true` plus a non-zero `invocations` is the proof this
   lane could not obtain.
3. **`RJdkHello` and `RStrings` must stay green.** Nothing in §2 touches the
   locale slot, but §2.4 changes a struct every `%t` conversion reads, and
   `RStrings` asserts `%tb` against `DateFormatSymbols.getInstance()`.
4. **`%<` is now a refusal.** Any `String.format` call whose format string
   OPENS with `%<` will start throwing `MissingFormatArgumentException` where
   it used to format `args[0]`. `grep -rn '"%<\|%<s\|%<d\|%<t' --include=*.rs
   --include=*.java .` was run over the whole tree at HEAD and found **no such
   call site**. The five live hits all have a preceding conversion and are
   unaffected:
   `regression-suite/src/RJdkIntrinsics2.java:2618` (`"%s %<s"`, and it
   ASSERTS `"x x"`), `probes/ShadowDifferentialProbe.java:2200`
   (`"%s-%<s"`), and three comment-only mentions in
   `native-builtins/src/lang_string.rs`, `native-builtins/src/preconditions.rs`
   and `types/src/error.rs` — the last two describing the JDK's OWN format
   string for `checkFromIndexSize`, which this VM renders with Rust's
   `format!` and not with `java.util.Formatter`.
5. **The four new `java.time` decoder arms invoke methods by name.**
   `Year.getValue()`, `Month.getValue()`, `YearMonth.getYear()`/
   `getMonthValue()`, `MonthDay.getMonthValue()`/`getDayOfMonth()`,
   `OffsetTime.getHour()`/`getMinute()`/`getSecond()`/`getNano()`/
   `getOffset()`. `invoke_i32` answers 0 for a method a class does not have,
   so a misspelling here is a SILENT wrong number, not a crash — and it would
   surface as `%tm` of a `Month` printing `00`. The regression vector in §6
   covers exactly that.
