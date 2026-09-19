# W8-F4-1 — `%h` was an alias for `%s`, and the sweep behind it found the null argument had never taken the JDK's printer

**Status:** OPEN (edits landed, unbuilt — every CratonVM "after" below is
**PREDICTED**, and every "HotSpot" value is **MEASURED** from a pasted `java`
transcript on Microsoft OpenJDK 25.0.3+9, 2026-08-13).

**File:** `native-builtins/src/lang_string.rs` — the sole
`java.util.Formatter` conversion table in the tree. Confirmed sole: the only
other `'h'` arms in the crate are `date_format_fast.rs`, `util_time.rs`
(`SimpleDateFormat` pattern letters) and `lib.rs:3839` (`Duration.parse` unit
letters), none of them a `%`-conversion, and `jit/` has no format table at all.

**Entry point:** `RJdkIntrinsics2 --only=strfmt`, which stopped at check 103 of
114 with

    %h must be the hex of hashCode() — 97 is 0x61

---

## 1. The reported defect

`java.util.Formatter.printHashCode` is four lines and this was the whole of it:

```java
String s = (arg == null ? "null" : Integer.toHexString(arg.hashCode()));
print(fmt, s, l);
```

`format_arg` answered instead:

```rust
// %h / %H: hashcode hex (left as-is — String fast path or "null").
if spec == 'h' || spec == 'H' {
    return Ok(ctx.read_string(*obj).unwrap_or_else(|| "null".to_string()));
}
```

The comment claims a hashcode hex and the body reads the argument's **text**.
Two wrong answers, and the second is the quieter one:

| | HotSpot 25 (measured) | CratonVM before | CratonVM after (predicted) |
|---|---|---|---|
| `%h` of `"a"` | `61` | `a` | `61` |
| `%h` of `"abc"` | `17862` | `abc` | `17862` |
| `%h` of `Integer.valueOf(42)` | `2a` | `null` | `2a` |
| `%h` of `Boolean.TRUE` | `4cf` | `null` | `4cf` |
| `%h` of `Double.valueOf(1.5)` | `3ff80000` | `null` | `3ff80000` |
| `%h` of a bean hashing `0xDEADBEEF` | `deadbeef` | `null` | `deadbeef` |

A `%h` of anything that is not a `String` printed `null` **with no refusal
anywhere** — `read_string` has nothing to read off an `Integer`, an enum or a
bean, and the `unwrap_or_else` turned that into a plausible-looking word.

The fix calls the VIRTUAL `hashCode`, which is the existing correct route
(`ctx.invoke_virtual(obj, "hashCode", "()I", &[])`, the same one
`intrinsics/record.rs:245` and `apps_h2.rs:474` take — no second helper was
added), and renders it UNSIGNED (`Integer.toHexString(-1)` is `ffffffff`, not
`-1`).

`%H` needed no separate arm: it is `%h` with the shared upper-caser, and that
is item 2.

---

## 2. `'H'` was missing from a four-entry remap, and that cost three answers

`format_arg_full` opens by folding the upper-case general conversions onto
their lower-case twins and upper-casing the result at the end. The list read
`'S' | 'B' | 'C'`. Everything downstream keys off the remapped `spec`, so one
missing line produced three unrelated-looking divergences:

| | HotSpot 25 (measured) | before | after (predicted) |
|---|---|---|---|
| `%H` of `"abc"` | `17862` (no letters — see next row) | — | `17862` |
| `%H` of a bean hashing `0xDEADBEEF` | `DEADBEEF` | `deadbeef` | `DEADBEEF` |
| `%H` of `null` | `NULL` | `null` | `NULL` |
| `%.2H` of `"abc"` | `17` | `17862` (precision arm matches `'s'/'b'/'h'`) | `17` |

This is the shape the brief predicted: a table populated from memory. It is
also why the sweep below was worth doing — the missing entry was not in the
conversion the vector named.

---

## 3. THE SWEEP — what a NULL argument does, which nothing here had ever asked

This is the largest finding and `%h` did not point at it; the table walk did.

Every printer in `java.util.Formatter` opens the same way —

```java
if (arg == null) { print(fmt, "null", l); return; }
```

— and `printBoolean` reaches the same `print(Formatter, String, Locale)` with
`"false"`. That shared printer does exactly three things: truncate to the
precision, upper-case when the conversion is an upper-case one, and
**space**-justify to the width. None of the numeric decoration applies,
because a null never enters a numeric printer at all.

CratonVM ran the null through the full flag pipeline. Measured on HotSpot 25,
all with `String.format(Locale.ROOT, fmt, (Object) null)`:

| format | HotSpot 25 | CratonVM before | after (predicted) |
|---|---|---|---|
| `%+d` | `null` | `+null` | `null` |
| `% d` | `null` | ` null` | `null` |
| `%#x` | `null` | `0xnull` | `null` |
| `%#o` | `null` | `0null` | `null` |
| `%08d` | `    null` | `0000null` | `    null` |
| `%08x` | `    null` | `0000null` | `    null` |
| `%08e` | `    null` | `0000null` | `    null` |
| `%#010x` | `      null` | `0x0000null` | `      null` |
| `%.2f` | `nu` | `null` | `nu` |
| `%,(.2f` | `nu` | `null` | `nu` |
| `%08.2f` | `      nu` | `0000null` | `      nu` |
| `%X` | `NULL` | `null` | `NULL` |
| `%E` | `NULL` | `null` | `NULL` |
| `%G` | `NULL` | `null` | `NULL` |
| `%A` | `NULL` | `null` | `NULL` |
| `%.3E` | `NUL` | `null` | `NUL` |
| `%.2B` | `FA` | `FALSE`* | `FA` |

\* `%.2B` was already right by accident (`'B'` remaps and the precision arm
matches `'b'`); the row is here because it is the one conversion whose null is
not the word `null`, and the new branch has to keep it that way.

The fix is one early branch in `format_arg_full` that IS that printer. Note
the two things it deliberately does not change: the flag LEGALITY checks still
run (`%08s` of null is still `FormatFlagsConversionMismatchException` on
HotSpot, and `fmt_check_spec` still raises it before this branch), and
`%.3X` of null is still `IllegalFormatPrecisionException` (an integral
conversion has no precision, argument or not).

**Why nothing caught it.** Every existing null row in the vectors is an
*undecorated* `%s` or `%b`. The decoration is what the null path skips, so a
bare `%s` of null agrees on every VM and covers none of the seventeen rows
above.

---

## 4. The `'('`/`'+'`/`' '` refusal on `%o`/`%x`/`%X` was hoisted out of the printer, and that refused what HotSpot prints

`checkInteger` does **not** reject those three flags. `print(long, Locale)`
does, in its own `%o` and `%x` arms — and `print(BigInteger, Locale)` has no
such check at all, calling `leadingSign`/`trailingSign` exactly as the decimal
arm does. `fmt_check_spec` had lifted the refusal up to the
argument-independent layer next to the `','` refusal (which really does belong
there), with the comment *"`print(long, Locale)`'s own check"* — the placement
named and then discarded.

Measured on HotSpot 25:

| format + argument | HotSpot 25 | CratonVM before | after (predicted) |
|---|---|---|---|
| `%(x` of `BigInteger("-255")` | `(ff)` | `FormatFlagsConversionMismatchException` | `(ff)` |
| `%+x` of `BigInteger("255")` | `+ff` | throw | `+ff` |
| `% o` of `BigInteger("8")` | ` 10` | throw | ` 10` |
| `%#x` of `BigInteger("-255")` | `-0xff` | throw | `-0xff` |
| `%#(x` of `BigInteger("-255")` | `(0xff)` | throw | `(0xff)` |
| `%(010x` of `BigInteger("-255")` | `(000000ff)` | throw | `(000000ff)` |
| `%#(016x` of `BigInteger("-255")` | `(0x0000000000ff)` | throw | `(0x0000000000ff)` |
| `%(x` of `(Object) null` | `null` | throw | `null` |
| `%(x` of `Long.valueOf(-255)` | throw, `Conversion = x, Flags = (` | throw ✓ | throw ✓ |
| `%(x` of `"s"` | `IllegalFormatConversionException` | `FormatFlagsConversionMismatch` (wrong class) | `IllegalFormatConversionException` |
| `%,x` of `BigInteger("255")` | throw (`,`) | throw ✓ | throw ✓ |

Three separate consequences of one hoist: a false refusal for BigInteger, a
false refusal for null, and the **wrong exception class** for a wrong-typed
argument (the flag check fired before `printInteger`'s dispatch could).

Admitting the sign made a second, dormant thing matter: the `'#'` radix
indicator was inserted at byte 0 unconditionally. The JDK appends it to a
builder `leadingSign` has already written into, so it goes INSIDE the sign —
`-0xff`, `(0xff)`. Invisible while every sign flag was refused.

---

## 5. `%b` read the unboxed shape instead of the class

> "If the argument arg is null, then the result is `"false"`. If arg is a
> boolean or Boolean, then the result is the string returned by
> `String.valueOf(arg)`. **Otherwise, the result is `"true"`.**"

`unbox_obj` collapses every integral wrapper to `Value::Int`, and the arm
tested that alone:

| | HotSpot 25 | before | after (predicted) |
|---|---|---|---|
| `%b` of `Integer.valueOf(0)` | `true` | `false` | `true` |
| `%b` of `Short.valueOf((short) 0)` | `true` | `false` | `true` |
| `%b` of `Byte.valueOf((byte) 0)` | `true` | `false` | `true` |
| `%b` of `Character.valueOf('\0')` | `true` | `false` | `true` |
| `%b` of `Long.valueOf(0L)` | `true` | `true` ✓ | `true` ✓ |
| `%b` of `Double.valueOf(0.0)` | `true` | `true` ✓ | `true` ✓ |
| `%b` of `Boolean.FALSE` | `false` | `false` ✓ | `false` ✓ |

An `Integer` of zero is a non-null object and nothing else. The four wrong
rows and the two accidentally-right ones have the same cause — which of them
`unbox_obj` happens to reduce to `Value::Int`.

---

## 6. `%n` had no precision check; `%%` beside it always did

`checkText` tests the precision **before** it switches on the conversion, so
the precision outranks both the width and the flag refusals.

| | HotSpot 25 | before | after (predicted) |
|---|---|---|---|
| `%.2n` | `IllegalFormatPrecisionException: 2` | formatted a line separator | throw |
| `%5.2n` | `IllegalFormatPrecisionException: 2` | `IllegalFormatWidthException: 5` | `IllegalFormatPrecisionException: 2` |
| `%-.2n` | `IllegalFormatPrecisionException: 2` | `IllegalFormatFlagsException: '-'` | `IllegalFormatPrecisionException: 2` |
| `%.2%` | `IllegalFormatPrecisionException: 2` | throw ✓ | throw ✓ |

One JDK rule written twice in the same `match`, in two adjacent arms, and one
of them drifted.

---

## 7. Width is measured in UTF-16 code units; this measured BYTES

`appendJustified` pads `width - cs.length()`, and a Java `length()` counts
UTF-16 code units. `str::len()` counts bytes. ASCII is where the two agree,
which is every existing row in every vector.

| | HotSpot 25 | before | after (predicted) |
|---|---|---|---|
| `%5s\|` of `"é"` | `␣␣␣␣é\|` (4 spaces) | 3 spaces | 4 spaces |
| `%5s\|` of `U+1F600` | `␣␣␣😀\|` (3 spaces, 2 code units) | 1 space | 3 spaces |
| `%-5s\|` of `"é"` | `é␣␣␣␣\|` | 3 spaces | 4 spaces |
| `%.3s` of `"a😀b"` | `a😀` (3 code UNITS) | `a😀b` (3 code POINTS) | `a😀` |

The same rule fixes `%t`'s justifier (`fmt_pad_to_width`), which had the same
byte count.

**Residual, unfixed and unfixable in this representation.** `%.1s` of `"😀"`
is a LONE HIGH SURROGATE on HotSpot (`length() == 1`); a Rust `String` cannot
hold one, so the new truncation stops before the pair and returns the empty
string. This is the same limit the `%c` arm already documents for a lone
surrogate code point (see `format_arg`, and `W7-41`).

---

## 8. Checked and CLEAR — the conversions the sweep confirms are right

A measured "these are correct" is the other half of the result. Every row
below was run on HotSpot 25 and traced through the Rust body; each is
**unchanged** by this lane and each **agrees**.

* **`%d`** — `Integer.MIN_VALUE` (`-2147483648`, no negation overflow); `Byte`,
  `Short`, `Long`, `BigInteger` including `-12345678901234567890`; `%,d` on
  both `Integer` and `BigInteger` (`1,234,567`); `%(d` of a negative
  `BigInteger` (`(42)`); `%08d` of `-314` → `-0000314` (zeros after the sign);
  `%,(d` of `-1234567` → `(1,234,567)`; `%(012d` of `BigInteger("-255")` →
  `(0000000255)`. Refusals: `%#d` mismatch, `%.2d` precision, `%+ d` →
  `IllegalFormatFlagsException: Flags = '+ '`, `%-0d`/`%0d`/`%-d` →
  `MissingFormatWidthException` quoting the specifier in canonical flag order
  (`%-0d`), `%d` of `String`/`Double`/`BigDecimal`/`Character`/`Boolean` →
  `IllegalFormatConversionException` naming the class.
* **`%o` / `%x` / `%X`** — unsigned two's complement at each width (`-1` as
  `int` → `ffffffff`, as `long` → `ffffffffffffffff`, as `Byte` → `377`/`ca`,
  as `Short` → `177777`/`ffff`); BigInteger sign-magnitude (`-ff`, `-FF`);
  `%#o` → `010`, `%#x` → `0xff`, `%#X` → `0XFF`, `%#010x` → `0x000000ff`,
  `%#010o` → `0000000010`; `%X` upper-cases the digits. Refusals: `%,x`/`%,o`
  mismatch (argument-independent, correctly still in `fmt_check_spec`),
  `%.2x` precision, `%x` of `Double`/`String`/`Character`/`BigDecimal` →
  `IllegalFormatConversionException`.
* **`%e` / `%E`** — `0.000000e+00` (six fraction digits, TWO-digit exponent);
  `%.3e` of `0.000123456` → `1.235e-04`; `%.0e` of `9.9` → `1e+01` (carry into
  the exponent, no point); `%E` of `1234.5` → `1.234500E+03`; `Float` widens;
  `BigDecimal` prints its own digits; `%.2e` of `-0.0` → `-0.00e+00`; `NaN`,
  `Infinity`, `-INFINITY`; `%010e` of `Infinity` → spaces not zeros. `%#e` is
  ALLOWED (`1.000000e+00`) and `%(e` gives `(1.000000e+00)`; `%,e` is a
  mismatch. All correct.
* **`%f`** — `-0.0` keeps its sign; `NaN`/`Infinity` ignore the precision;
  HALF_UP at `.0` for `0.5`/`1.5`/`2.5` → `1`/`2`/`3` (where the FP unit's
  even-rounding would say `2` at 2.5); `BigDecimal("2.3")` → `2.300000` and
  `%.2f` of `BigDecimal("2.345")` → `2.35` (the literal's digits, not the
  double's); `%,(.2f` of `-1234.5` → `(1,234.50)`; `%08.3f` of `-3.14` →
  `-003.140`; `%#f` allowed. All correct.
* **`%g` / `%G`** — `0.0001` → `0.000100000`; `1234567.0` → `1.23457e+06`;
  zero → `0.00000`; `%.0g` of `1.5` → `2`; `%G` upper-cases;
  `%,g`/`%(g` allowed, `%#g` a mismatch. All correct.
* **`%a` / `%A`** — `0x1.0p0`, `0X1.0P0`, `0x0.0p0`, `-0x1.0p0`; `%.2a` →
  `0x1.00p0`; `%020a` → `0x00000000000001.0p0` and `%+020a` →
  `+0x0000000000001.0p0` (zeros after BOTH the sign and the prefix); `%#a`
  allowed, `%,a`/`%(a` mismatches; `%a` of `BigDecimal` →
  `IllegalFormatConversionException` (the one float conversion that refuses
  one). All correct.
* **`%c` / `%C`** — `Character`, `Byte`, `Short`, `Integer` only (`Long` and
  `Float` → `IllegalFormatConversionException`); an astral code point emits a
  surrogate PAIR (`length() == 2`); `%C` upper-cases; `%c` of `0x110000` and of
  a negative `Byte` → `IllegalFormatCodePointException` (`Code point =
  0xffffffff` for the byte, unsigned in the message); `%.1c` →
  `IllegalFormatPrecisionException`; `%#c` mismatch; `%-c` missing width.
  All correct.
* **`%s` / `%S`** — `String.valueOf(arg)` via `toString()`, so beans, enums and
  `BigDecimal` print themselves; `%5.2s` truncates then pads; `%#s` is a
  mismatch reported as `Conversion = s` (raised at PRINT time, after
  `%#-s`'s missing-width, which is the ordering `checkGeneral` fixes);
  `%,s`/`%08s` mismatches. All correct.
* **`%b` / `%B`** — the null and non-Boolean rules, once §5 lands; `%.2b` →
  `tr`; `%#b`/`%08b` mismatches; `%-b` missing width. Correct.
* **`%t` / `%T`** — `%tY`/`%tb`/`%TB` off a `Calendar` and a `Long`; the
  prefix is only a prefix when a field follows, so `%t` and `%t1` both report
  `Conversion = 't'` while `%tq` reports `Conversion = 'tq'` and `%t%` reports
  `Conversion = 't%'`; `%.2tY` precision; `%,tY`/`%#tY` mismatch naming the
  FIELD (`Conversion = Y`); `%-tY` missing width quoting `%-tY`; null → `null`
  and `%TY` of null → `NULL`. All correct.
* **`%n` / `%%`** — `%n` is `System.lineSeparator()` (`\r\n` here, byte-for-byte);
  `%5n` → `IllegalFormatWidthException`, `%-n` → `IllegalFormatFlagsException`;
  `%5%` → `    %`, `%-5%` → `%    `, `%-%` missing width, `%#%` illegal flags,
  `%.2%` precision. Correct apart from §6.
* **Parser** — `%q`/`%Q` report the character AS TYPED (never folded);
  `%d` with no argument → `MissingFormatArgumentException: Format specifier
  '%d'`; `%0$s` → `IllegalFormatArgumentIndexException`; `%2$s %1$s` reorders;
  `%s %<s` re-uses; `String.format(locale, (String) null)` →
  `NullPointerException`. All correct.

---

## 9. Which `--only=strfmt` checks should flip

The block is 114 checks and `check()` throws on the first failure, so it
stopped at the `%h` row. **Twelve checks have never executed on any CratonVM
binary.** Traced, they are:

| # | check | predicted |
|---|---|---|
| 103 | `%h` of `"a"` == `61` | **FLIPS to PASS** — §1 |
| 104 | `%h` of null == `null` | PASS (was already right; now via §3's branch) |
| 105 | `%5.2s` of `"abcdef"` == `   ab` | PASS |
| 106 | `%,(.2f` of `-1234.5` == `(1,234.50)` | PASS |
| 107 | `%08.3f` of `-3.14` == `-003.140` | PASS |
| 108 | `%.3e` of `0.000123456` == `1.235e-04` | PASS |
| 109 | `%.0e` of `9.9` == `1e+01` | PASS |
| 110 | `%s %<s` of `"x"` == `x x` | PASS |
| 111 | `%.2d` → `IllegalFormatPrecisionException` | PASS |
| 112 | `%0$s` → `IllegalFormatArgumentIndexException` | PASS |
| 113 | `%c` of `0x110000` → `IllegalFormatCodePointException` | PASS |
| 114 | `String.format(ROOT, (String) null)` → `NullPointerException` | PASS |

**Predicted headline:** `CK RJdkIntrinsics2 strfmt=114`.

**What I expect to surface behind the wall, and did not find:** nothing. That
is the honest answer and it is worth stating, because the brief's premise —
"checks beyond this one may never have executed" — was right about the
*execution* and wrong about the *risk*. Every one of the eleven checks behind
`%h` exercises a path some earlier check in the same block already reached
(`%.2s` behind `%S`, `%,(.2f` behind `%,d` and `%(d`, `%.3e` behind `%e`,
`%.2d` behind `%#d`). The block's own ordering front-loaded the novel rows.
**The novel divergences are the ones `strfmt` does not test at all** — §3
(seventeen rows), §4 (eleven), §5 (four), §6 (three), §7 (four) — and they
were found by walking the JDK's table, not by unblocking the vector.

**Confidence caveat.** Checks 111–113 assert exact exception CLASS names and
therefore depend on `fmt_exception_class_available` resolving
`IllegalFormatPrecisionException`, `IllegalFormatArgumentIndexException` and
`IllegalFormatCodePointException` rather than falling back to
`IllegalArgumentException`. In `--real-jdk`/`--jdk-only` those are real JDK
classes and the fallback should not fire; in `--features synthetic-jdk` it may.
If one of those three is the row that stays red, the defect is the exception
CLASS's availability and not the conversion.

---

## 10. NOMINATIONS

* **N1 — `%s` of a `Formattable` never dispatches `formatTo`.** MEASURED:
  `printString` opens `if (arg instanceof Formattable) { ((Formattable) arg).formatTo(fmt, flags, width, precision); }`,
  and passes the flags as the JDK's INTERNAL bit set — `%s` → 0, `%#s` → 4,
  `%S` → 2 on HotSpot 25. CratonVM calls `toString()` instead, so a
  `Formattable` prints its `toString` and the flags/width/precision never reach
  it. Not fixed here: it needs `java/util/FormattableFlags`' bit values and a
  `java.util.Formatter` receiver to hand the callee, which is a different
  surface from the conversion table. `%#s` of a `Formattable` is also the one
  case where the `'#'` mismatch must NOT fire.
* **N2 — `%.1s` of a supplementary character cannot answer.** See §7's
  residual. Fixing it needs a `String` representation that can hold an unpaired
  surrogate — the same blocker `%c` records. Two arms now cite it; a third
  will appear.
* **N3 — the general family's upper-caser ignores the LOCALE.**
  `print(Formatter, String, Locale)` calls
  `s.toUpperCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT)))`.
  MEASURED on HotSpot 25:
  `String.format(Locale.forLanguageTag("tr"), "%S", "i")` is **U+0130** where
  `Locale.ROOT` gives `I`, and `%C` of `Character.valueOf('i')` in `tr` is
  **U+0130** likewise. `format_arg_full` ends with a bare
  `formatted.to_uppercase()` — Rust's locale-independent mapping — and the new
  `'H'` entry and the §3 null branch both inherit it, so this lane widened the
  row by two conversions without widening the defect's cause. The correct
  helper already exists in this crate: `case_map.rs` owns `tr`/`az`/`lt`, and
  the general family does not call it. Same species as the
  `java_char_is_whitespace` note fifty lines away in the same file — one rule,
  two implementations, one of them reaching for Rust's. The CratonVM half is
  NOT MEASURED (I do not run the binary); the HotSpot half above is.
* **N4 — `format_arg`'s `Value::Object(None)` arm is now dead** on the
  `String.format` path (§3 returns before it) but is still correct and still
  `pub(crate)`. Left in place deliberately. If a future lane makes `format_arg`
  the entry point for something else, that arm is the one that will answer, and
  it does NOT apply the precision or the upper-casing — it is the OLD null
  behaviour preserved in a corner. Worth deleting or worth routing; not both
  ways.
* **N6 — a large-but-valid width is an unbounded allocation, and the guard
  next to it says the problem is solved.** The width parser refuses only a run
  of digits that overflows `i32` (`IllegalFormatWidthException(Integer.MIN_VALUE)`),
  and its comment cites `%2147483648d` as the case it closed. `%2000000000d`
  parses cleanly and reaches `" ".repeat(2_000_000_000)` — two gigabytes, then
  `create_string` on top of it. On HotSpot the same format string builds a
  `StringBuilder` and raises a catchable `OutOfMemoryError`; a Rust allocation
  failure ABORTS the process, so the failure modes are not the same class of
  event even where both fail. NOT MEASURED on either VM (I did not want a
  multi-gigabyte allocation in a shared worktree), which is exactly why it is a
  nomination and not a row above. The guard to add is a cap tested against the
  format string's own length, not a wider integer type.
* **N5 — the width justifier is duplicated.** `fmt_pad_to_width` (the `%t`
  path) and the `if let Some(w) = width` block in `format_arg_full` are the
  same `appendJustified`, written twice. They now agree on the UTF-16 count
  because this lane changed both; they did not agree before and nothing makes
  them agree next time.
