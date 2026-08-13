# W8-F13-1 — `%s` of a `Formattable` never dispatched, and walking the printer behind it found the upper-caser running AFTER the width justifier

**Status:** OPEN (edits landed, unbuilt — every CratonVM "after" below is
**PREDICTED**, and every "HotSpot" value is **MEASURED** from a pasted `java`
transcript on Microsoft OpenJDK 25.0.3+9, 2026-08-13).

**File:** `native-builtins/src/lang_string.rs` — the sole `java.util.Formatter`
conversion table in the tree (confirmed sole by
`W8-F4-1-the-formatter-conversion-table-swept-against-the-spec.md` §intro).

**Prior record:** W8-F4-1, whose nominations N1, N2, N3, N4, N5 and N6 are the
brief for this lane. Its method is the one used here: walk the JDK's source,
not the vector. Four of the seven findings below are **not** in any nomination
and not in `--only=strfmt`.

**Method note.** `printString` is six lines. Reading all six is what produced
§1, §2 AND §3 — the nomination named only the first.

---

## 1. `%s` of a `Formattable` never dispatched `formatTo` (N1)

`java.util.Formatter.printString`, in full:

```java
private void printString(Formatter fmt, Object arg, Locale l) throws IOException {
    if (arg instanceof Formattable) {
        if (fmt.locale() != l)
            fmt = new Formatter(fmt.out(), l);
        ((Formattable)arg).formatTo(fmt, flags, width, precision);
    } else {
        if (Flags.contains(flags, Flags.ALTERNATE))
            failMismatch(Flags.ALTERNATE, 's');
        if (arg == null)  print(fmt, "null", l);
        else              print(fmt, arg.toString(), l);
    }
}
```

CratonVM went straight to `toString()`. The `formatTo` contract — the whole
reason `Formattable` exists — was unreachable, and the flags, width and
precision the callee is supposed to interpret never left this file.

Measured on HotSpot 25 with a `Formattable` whose `formatTo` prints its own
three arguments:

| format | HotSpot 25 `formatTo(_, flags, width, precision)` | CratonVM before | after (predicted) |
|---|---|---|---|
| `%s` | `f=0 w=-1 p=-1` | `toString()` | dispatch, `f=0 w=-1 p=-1` |
| `%S` | `f=2 w=-1 p=-1` | `TOSTRING` (upper-cased) | dispatch, `f=2` |
| `%#s` | `f=4 w=-1 p=-1` | **`FormatFlagsConversionMismatchException`** | dispatch, `f=4` |
| `%#S` | `f=6 w=-1 p=-1` | throw | dispatch, `f=6` |
| `%-10s` | `f=1 w=10 p=-1` | `toString()` padded to 10 | dispatch, `f=1 w=10` |
| `%10s` | `f=0 w=10 p=-1` | padded to 10 | dispatch, `w=10` |
| `%.3s` | `f=0 w=-1 p=3` | `toString()` truncated to 3 | dispatch, `p=3` |
| `%#-10.3S` | `f=7 w=10 p=3` | throw | dispatch, `f=7 w=10 p=3` |
| `%s %<s` | second spec is `f=**256**` | `toString()` twice | `f=0`, then `f=256` |

Three things the table above pins down and which a table written from memory
would get wrong:

* **absent width/precision are `-1`, not `0`.** The callee's own `if (width ==
  -1)` is the normal shape of a `formatTo`, so a `0` would make it pad.
* **`256` is not a `FormattableFlags` constant.** It is `Flags.PREVIOUS`, the
  package-private `'<'` bit, and it reaches a `Formattable` because
  `checkGeneral` removes `PLUS | LEADING_SPACE | ZERO_PAD | GROUP |
  PARENTHESES` and nothing else. The public `FormattableFlags` javadoc lists
  three constants; the argument carries four bits.
* **the JDK hands over a `Formatter` carrying THIS CALL's locale.** Measured:
  `String.format(Locale.ROOT, …)` → the callee sees `Locale.ROOT`;
  `String.format((Locale) null, …)` → the callee sees **`null`**;
  `String.format(String, Object...)` → the callee sees the default FORMAT
  locale (`ru_RU` on this host).

Also measured, and each one is a separate rule the fix has to keep:

| | HotSpot 25 | after (predicted) |
|---|---|---|
| `"[%s]"` of a `formatTo` that writes nothing | `[]` | `[]` |
| `"[%10s]"` of the same | `[]` — the width is **not** applied afterwards | `[]` |
| `%s` of a `formatTo` that throws `IllegalStateException` | propagates out of `String.format` | propagates |
| `%b` of the same object | `true` — `printBoolean` has no such branch | `true` ✓ |
| `%h` of the same object (hashing `0xCAFE`) | `cafe` — nor does `printHashCode` | `cafe` ✓ |
| `%s` of a SUBCLASS of a `Formattable` | dispatches | dispatches |
| `%s` of a class implementing an interface that EXTENDS `Formattable` | dispatches | dispatches |
| `%s` of a `formatTo` that calls `String.format` re-entrantly | `1,234,567` | `1,234,567` |

The dispatch is `fmt_formattable_dispatch`. It builds a `StringBuilder`, a
`java.util.Formatter` over it via the existing
`(Ljava/lang/Appendable;Ljava/util/Locale;)V` constructor (the one
`native-builtins/src/lib.rs:21470` deliberately does NOT shadow, for exactly
this reason — the real constructor also writes `zero`), calls `formatTo`, and
reads the builder back. `java/util/Formattable` is looked up with
`class_id_by_name` and **never loaded**: for the argument to implement the
interface, the interface must already have been resolved when the argument's
own class was linked, so a miss is a conclusive "no" and the hot `%s` path pays
one index read. A VM with no usable `java.util.Formatter` (synthetic-jdk)
declines and prints `toString()`, which is the old behaviour.

---

## 2. The `'#'` refusal for `%s` had been hoisted out of the printer, and that refused what HotSpot prints

Same species as W8-F4-1 §4, in the conversion next door. `checkGeneral` does
**not** reject `ALTERNATE` for `'s'` — `printString`'s `else` does, after the
`Formattable` test and after the argument has been fetched. `fmt_check_spec`
raised it at the argument-independent layer.

Placement decides three answers, all measured:

| | HotSpot 25 | CratonVM before | after (predicted) |
|---|---|---|---|
| `%#s` of a `Formattable` | dispatches, `flags=4` | `FormatFlagsConversionMismatchException` | dispatches |
| `%#s` with NO argument | `MissingFormatArgumentException: Format specifier '%#s'` | `FormatFlagsConversionMismatch` (wrong class) | `MissingFormatArgument` |
| `%#s` of a `String` | `FormatFlagsConversionMismatchException: Conversion = s, Flags = #` | throw ✓ | throw ✓ |
| `%#s` of `(Object) null` | throw (the `'#'` test precedes the null test) | throw ✓ | throw ✓ |

And three orderings that must NOT move, also measured, and which is why only
the `'#'` line was relocated:

* `%#,s` → `Conversion = s, Flags = ,` (`checkGeneral`'s `checkBadFlags`, still
  argument-independent).
* `%#-s` → `MissingFormatWidthException: %-#s` (the width test comes first
  inside `checkGeneral`).
* `%#0s` → `Conversion = s, Flags = 0`.
* `%#b` / `%#h` → still refused up front, because `checkGeneral` really does
  test `ALTERNATE` first for those two.

---

## 3. THE FINDING THE NOMINATIONS DID NOT NAME — the general family's upper-caser ran AFTER the width justifier

`print(Formatter, String, Locale)` is three statements in this order:

```java
if (precision != -1 && precision < s.length()) s = s.substring(0, precision);
if (Flags.contains(flags, Flags.UPPERCASE))    s = toUpperCaseWithLocale(s, l);
appendJustified(fmt.a, s);
```

`format_arg_full` did the upper-casing as its **last** statement, after the
width padding. Invisible for every ASCII row, because ASCII upper-casing is
length-preserving. It is not length-preserving in general: `String.toUpperCase`
applies the FULL mapping, and `U+00DF` (sharp s) becomes `"SS"`.

| | HotSpot 25 | CratonVM before | after (predicted) |
|---|---|---|---|
| `%5S` of `"ß"` | `␣␣␣SS` (5 units) | `␣␣␣␣SS` (**6**) | `␣␣␣SS` |
| `%-5S\|` of `"ß"` | `SS␣␣␣\|` | `SS␣␣␣␣\|` | `SS␣␣␣\|` |
| `%5S` of `"aßb"` | `␣ASSB` (5) | `␣␣ASSB` (6) | `␣ASSB` |
| `%5C` of `Character.valueOf('ß')` | `␣␣␣SS` | `␣␣␣␣SS` | `␣␣␣SS` |
| `%.1S` of `"ß"` | `SS` — the precision cuts the SOURCE, then the mapping grows past it | `SS` ✓ | `SS` ✓ |
| `%5s` of `"ß"` (no upper-casing) | `␣␣␣␣ß` | `␣␣␣␣ß` ✓ | ✓ |

Any width whose field the mapping overflows was silently one unit too wide.
The same reordering was applied to `%T` (§4) and it is what `fmt_pad_to_width`
now sits at the end of.

**Why nothing caught it.** Every `%S`/`%C`/`%B`/`%H` row in every vector is
ASCII, and the only conversion in the general family whose *unpadded* output
can change length is one nobody writes a test for.

---

## 4. The upper-caser ignored the LOCALE, in four places rather than one (N3)

`toUpperCaseWithLocale(s, l)` is
`s.toUpperCase(Objects.requireNonNullElse(l, Locale.getDefault(FORMAT)))`.
Three call sites in this file reached for Rust's locale-independent
`str::to_uppercase`; a fourth reached for it inside `%tr`.

Measured on HotSpot 25:

| | `Locale.ROOT` | `tr` | `az` | `lt` |
|---|---|---|---|---|
| `%S` of `"i"` | `I` | **`U+0130`** | **`U+0130`** | `I` |
| `%C` of `Character.valueOf('i')` | `I` | **`U+0130`** | **`U+0130`** | `I` |
| `%.1S` of `"ii"` | `I` | **`U+0130`** | **`U+0130`** | `I` |
| `%S` of `(Object) null` | `NULL` | `NULL` | `NULL` | `NULL` |
| `%TA` of a Monday | `MON` | **`PAZARTES` + `U+0130`** | — | — |
| `%TB` of January | `JAN` | `OCAK` | — | — |

and the explicit-null / absent-locale question, measured by forcing
`Locale.setDefault(Locale.Category.FORMAT, tr)`:

| | before forcing (`FORMAT=ru_RU`) | with `FORMAT=tr` |
|---|---|---|
| `String.format((Locale) null, "%S", "i")` | `I` | **`U+0130`** |
| `String.format("%S", "i")` | `I` | **`U+0130`** |
| `String.format(Locale.ROOT, "%S", "i")` | `I` | `I` |

So an **explicit null locale takes the DEFAULT locale's case rules** — the
opposite of what the same null means for the separators, where "no
localization is applied" and `fmt_symbols_for` correctly answers the root
constants. One `Locale` argument, two questions, two different answers; the new
`fmt_upper_case` folds `Given(None)` in with `DefaultFormat` and
`fmt_symbols_for` does not, and the two now say so in each other's doc
comments.

The rules are **not reimplemented**: `case_map::to_upper_case(s, lang)` is
already `pub`, already owns the `tr`/`az`/`lt` port of the JDK's
`ConditionalSpecialCasing`, and `String.toUpperCase(Locale)` is already its
caller. **No visibility nomination was needed** — the entry point was public.
The language is resolved through the existing
`crate::locale_language_for_case_mapping`, the same helper `string_case_impl`
uses, which runs no bytecode (a field read off the `Locale`, or a `OnceLock`'d
`user.language`), so there is no re-entrancy hazard and no latch.

Root-locale output changes too, in one direction only: `str::to_uppercase`
becomes `case_map::jdk_to_uppercase`, which carries the six-code-point
JDK-version-skew correction `String.toUpperCase` has had since W7-95a.
Measured, both locales agree: `%S` of `U+A7CC` is `U+A7CC` on HotSpot under
ROOT and under `tr`.

**Which numeric conversions do NOT need this, and why the sweep says so rather
than guessing.** `%X` of a long and of a `BigInteger` genuinely call
`toUpperCaseWithLocale` (Formatter.java:3542, 3617) — but their content is hex
digits, and no locale maps `a`..`f` differently; measured `%X` of `-1L` is
`FFFFFFFFFFFFFFFF` under ROOT, `tr`, `az` and `lt` alike. `%E`/`%G`'s `NAN` and
`INFINITY` are literals. `%A` upper-cases with an explicit `Locale.ROOT` and the
JDK says why in a comment ("don't localize hex"); measured `0X1.0P0` in all
four locales. All four left alone.

---

## 5. `%2000000000d` was an unbounded allocation, and so is `%.2000000000f` (N6, and its twin)

N6 marked this **unmeasured on both VMs**. It is measured now, in a subprocess
with a timeout, on HotSpot 25:

| format | `-Xmx256m` | `-Xmx1g` |
|---|---|---|
| `%2147483647d` of `1` | `OutOfMemoryError: Java heap space` (1.6 s) | — |
| `%2000000000d` of `1` | `OutOfMemoryError: Java heap space` (1.5 s) | — |
| `%1000000000d` of `1` | `OutOfMemoryError: Java heap space` (2.2 s) | — |
| `%2147483648d` of `1` | `IllegalFormatWidthException: -2147483648` (4 ms) | — |
| `%100000000d` of `1` | — | **SERVED**, `length=100000000` (1.8 s) |
| `%100000000s` of `1` | — | **SERVED**, `length=100000000` |
| `%100000000%` | — | **SERVED**, `length=100000000` |

**That last block is the reason the guard is not a cap.** A hundred-million
character field is a legal answer HotSpot gives; the failure is not a property
of the format string, it is the allocator refusing. N6's proposed guard — "a
cap tested against the format string's own length" — would refuse
`%100000000d`, which is a divergence in the other direction. The guard taken
instead is a **fallible allocation**: `fmt_repeat` reserves with
`try_reserve_exact` (the same primitive `native_string_repeat` twenty screens
away already uses) and raises `OutOfMemoryError: Java heap space`, HotSpot's
own message, when the reserve fails.

Every padding run in the file now goes through it — the space justifier, the
`trailingZeros` zero run, and the `%%` conversion's own padding, which was a
fourth `str::repeat` nobody had counted.

**The twin N6 did not name.** The precision is the same axis. Measured, same
subprocess, `-Xmx256m`, with a `Double` argument:

| format | HotSpot 25 |
|---|---|
| `%.2000000000f` | `OutOfMemoryError: Java heap space` (59 ms) |
| `%.2000000000e` | `OutOfMemoryError: Java heap space` (287 ms) |
| `%.2000000000a` | `OutOfMemoryError: Java heap space` (55 ms) |
| `%.1000000f` | **SERVED**, `length=1000002` |

`fmt_render_fixed`, `fmt_render_scientific` and `fmt_hex_pad` each push one
character per fraction digit into a growing `String`, which is a
`handle_alloc_error` abort at two billion. There is exactly one point at which
a precision reaches any of the three — the float arm of `format_arg_full` —
so a single `fmt_reserve_probe(precision)` closes all three without making any
of their signatures fallible.

CratonVM's before-state on all seven rows above is **an abort**, not a value:
`str::repeat`'s capacity path calls `handle_alloc_error`, and a Rust abort is
not a Java throwable. `%.2000000000s` is not affected (a precision truncates,
it does not pad) and is `length=1` on HotSpot.

**Residual, stated rather than papered over.** `try_reserve_exact` fails only
when the OS refuses. On a host with a large page file a two-gigabyte reserve
may SUCCEED, and the resulting string is then handed to
`create_string_uninterned`, which allocates in the VM heap and is not this
file's to guard. See NOMINATIONS.

---

## 6. The width justifier was written twice; there is now one (N5)

`fmt_pad_to_width` (the `%t` and null path) and the `if let Some(w) = width`
block inside `format_arg_full` were the same `appendJustified`. They are now
one: `format_arg_full` ends with `fmt_pad_to_width(formatted, flags, width)`.

**How I checked the survivor carries W8-F4-1's UTF-16 fix**, since that is the
question a consolidation has to answer and not assert:

1. `fmt_pad_to_width`'s body is five lines and contains no `str::len` — its
   two length reads are both `fmt_utf16_len(&out)`, which is
   `s.chars().map(char::len_utf16).sum()`.
2. `grep -n '\.repeat(' lang_string.rs` no longer reaches any width-driven
   site. The only two executable survivors in the whole file are
   `fmt_hex_pad`'s precision-driven runs at 8437 and 8442, which §5's probe
   covers; the remaining hits are doc comments (`String.repeat`,
   `StringBuilder.repeat`).
3. `grep -rn 'fmt_pad_to_width(' --include=*.rs .` returns one definition and
   four calls, all in this file — so every justification decision the formatter
   makes now runs the same five lines.
4. The two rows that made the byte count visible in the first place —
   `%5s\|` of `"é"` (`␣␣␣␣é\|`, MEASURED) and of `U+1F600` (`␣␣␣😀\|`, MEASURED,
   three spaces for two code units) — are answered by that body.

**What was deliberately NOT merged.** The zero padding. `trailingZeros` runs
*inside* the JDK's numeric printers, before `appendJustified` sees the buffer,
and the buffer is `width` units long by the time it does — so it is a separate
step that leaves the shared justifier a no-op, not a second justifier. Folding
it in would have re-created the duplication in a new shape. The restructure is
behaviour-preserving on every existing row: `zero_pad` is already
`flags.contains('0') && !left_justify`, so the arm the old `else if` guarded is
the arm the new `if` guards.

One consequence worth naming: the space padding now happens **after**
`fmt_localize` rather than before it. Spaces are not digits and `fmt_localize`
maps only `0`–`9`, `,` and `.`, so the answer is identical; the zero padding
still happens BEFORE it, which is load-bearing, because `trailingZeros` appends
the locale's own zero digit and `fmt_localize` is what supplies it.

---

## 7. `format_arg`'s dead null arm is now ROUTED, not both ways (N4)

N4: "Worth deleting or worth routing; not both ways." Routed. The word a null
prints — `"false"` for `%b`/`%B`, `"null"` for everything else — is now
`fmt_null_text`, and both the live printer in `format_arg_full` and the dead
arm in `format_arg` read it. Deleting the arm was rejected: the enclosing
`match` has a `_ => "?"` catch-all, so a deletion would answer `?` for a null
if the function ever became an entry point again — strictly worse than the old
behaviour it preserves.

`format_arg` is still `pub(crate)` and still has no caller outside this file
(`grep -rn 'format_arg(' --include=*.rs .` → four hits, all in `lang_string.rs`,
two of them its own recursion). Narrowing it was NOT done: another lane adding
a caller mid-flight would turn a stale visibility into a build break in a shared
worktree. See NOMINATIONS.

---

## 8. `%.1s` of a supplementary character — the third arm to cite the same blocker (N2)

Confirmed, not fixed. Measured on HotSpot 25:

| | HotSpot 25 | CratonVM (unchanged) |
|---|---|---|
| `%.1s` of `U+1F600` | a LONE HIGH SURROGATE, `length() == 1` | `""` |
| `%.2s` of `U+1F600` | the pair, `length() == 2` | the pair ✓ |
| `%.1S` of `U+1F600` | the lone high surrogate | `""` |
| `[%5.1s]` of `U+1F600` | `[␣␣␣␣<D83D>]`, `length() == 7` | `[␣␣␣␣␣]` |
| `%.3s` of `"a"+U+1F600+"b"` | `a😀` (3 code UNITS) | `a😀` ✓ |

`fmt_truncate_utf16` stops before the pair because a Rust `String` cannot hold
an unpaired surrogate. This is the third arm to record the same limit — after
`format_arg`'s `%c` (W7-41) and `native_string_repeat`'s lossy `from_utf16_lossy`
— and all three need the same one thing. Renominated below with the API.

---

## 9. Checked and CLEAR — what the walk confirms is already right

Each row below was run on HotSpot 25 and traced through the Rust body. None is
changed by this lane and each agrees.

* **`printBoolean` / `printHashCode` do not dispatch `Formattable`.** `%b` of a
  `Formattable` is `true`, `%h` is the hex of its `hashCode`, `%B`/`%H` are the
  upper-cased forms. CratonVM reaches the same arms — the new dispatch is
  gated on `spec == 's'` after the remap, so `'S'` reaches it and `'b'`/`'h'`
  cannot.
* **`%s` of a plain object** still `toString()`s, including through the new
  interface probe: a `%s` whose argument is not a `Formattable` costs one
  `class_id_by_name` and one `is_subclass` and takes the identical path.
* **The null path's own ordering** — truncate, upper-case, justify — was
  already in the JDK's order, so §3's reorder does not disturb it: `%8X` of
  null is `␣␣␣␣NULL` and `%-8X\|` is `NULL␣␣␣␣\|` (measured), `%08.2f` of null
  is `␣␣␣␣␣␣nu`.
* **The W8-F4-1 rows this lane sits on top of**, re-measured to confirm the
  restructure preserves them: `%5.2s` of `"abcdef"` → `␣␣␣ab`; `%,(.2f` of
  `-1234.5` → `(1,234.50)`; `%08.3f` of `-3.14` → `-003.140`; `%.3e` of
  `0.000123456` → `1.235e-04`; `%.0e` of `9.9` → `1e+01`; `%s %<s` of `"x"` →
  `x x`; `%h` of `"a"` → `61`; `%h` of null → `null`; `%.2H` of `"abc"` → `17`;
  `%X`/`%08d`/`%.2f` of null → `NULL` / `␣␣␣␣null` / `nu`; `%(x` of
  `BigInteger("-255")` → `(ff)`; `%#(016x` of the same → `(0x0000000000ff)`;
  `%b` of `Integer.valueOf(0)` → `true`; `%.2n` and `%5.2n` →
  `IllegalFormatPrecisionException: 2`.
* **`%S` of `"ab"`** is `AB` under every locale tested, which is the one
  `--only=strfmt` row the locale change could have moved. It cannot: `a`/`b`
  have no locale-dependent mapping.
* **`%tr`'s AM/PM** is `toUpperCaseWithLocale` in the JDK (Formatter.java:4381)
  and was `str::to_uppercase` here — corrected, though no measured row
  currently separates them (English/Turkish `AM`/`PM` are ASCII).

---

## 10. Which `--only=strfmt` rows this lane touches

**None.** Stated as a result, not a hedge: W8-F4-1 predicts the family reaches
`strfmt=114` from its changes alone, and nothing here moves a row off that.

Traced individually, the block's format-family checks are `%.1f` of `-0.0`,
`%e`, `%g` (×2), `%d` of `Integer.MIN_VALUE`, `%x` (×2), `%,d`, `%08.3f`,
`%-8d|`, `%+d`, `%(d`, `%,.2f` under `Locale.GERMANY`, `%.2f` of NaN and
Infinity, `%.0f` (×3), `%s`/`%b` of null, `%b` of a String, `%S` of `"ab"`,
`%2$s %1$s`, `%%`, `%n`, `"%s!".formatted`, the `{"%q","%d"}` bad-format pair,
`%d` of a String, and W8-F4-1's checks 103–114. Every one of them is ASCII, has
no `Formattable` argument, and carries neither a `'#'` on `%s` nor a width a
case mapping could overflow. The three restructures that could in principle
move a row are each behaviour-identical on them:

* the justifier merge — `%08.3f` (zero pad), `%-8d|` (left justify) and
  `%,.2f` (localize) are the three shapes it touches, and all three take the
  same branch as before;
* the `'#'`-for-`%s` relocation — no strfmt row writes `%#s`, and the
  `badFmt` table is `{"%q", "%d"}`;
* the locale upper-caser — `%S` of `"ab"`, above.

The novel divergences are, again, the ones the vector does not test: §1 (nine
rows plus eight behaviours), §2 (four), §3 (six), §4 (thirteen), §5 (eleven).

---

## 11. NOMINATIONS

* **N1 — `create_string_from_utf16` on `NativeContext`.** Third arm, and the
  first one to be able to say what the API is: `vm/src/vm/vm_object.rs:758`
  already has the lossless READER (`read_java_string_units` → `Vec<u16>`), and
  there is no writer — `create_string_uninterned` and
  `create_string_uninterned_gc_safe` both take `&str`. Until there is one,
  `%.1s` of a supplementary character (§8), `%c` of a lone surrogate code point
  (W7-41) and `String.repeat` of a string containing one
  (`native_string_repeat`'s `from_utf16_lossy`) are all unanswerable, and all
  three are one function away. NOT attempted here: `vm/` is not this lane's.

* **N2 — the VM-heap half of the allocation guard.** §5 converts the Rust-side
  abort into a catchable `OutOfMemoryError`, but only when the ALLOCATOR
  refuses. A host that can hand out two gigabytes then passes a
  2,000,000,000-character `String` to `ctx.create_string_uninterned`, which
  allocates in the Java heap and has no fallible variant on the
  `NativeContext` trait. The nomination is a `try_create_string` (or a
  documented maximum) next to `create_string_uninterned`, so a native can turn
  a heap refusal into a throwable instead of whatever the heap does. UNMEASURED
  on CratonVM — I do not run the binary — and the HotSpot half is §5's table.

* **N3 — `format_arg`'s `pub(crate)` is stale.** No caller outside
  `lang_string.rs`; §7 explains why narrowing it was not done from a shared
  worktree. A lane that owns a quiet moment should make it private, at which
  point the compiler will keep the `format_arg`/`format_arg_full` split honest
  instead of a comment doing it.

* **N4 — the `Formattable` probe costs a hierarchy walk on every `%s`.**
  `fmt_formattable_dispatch` runs `class_id_by_name` + `is_subclass` for every
  object argument of every `%s`, and `is_subclass` takes the class-manager read
  lock. `%s` is the hottest conversion in the VM (every logging shim goes
  through `format_impl`). A `java/lang/String` fast-path would skip it for the
  overwhelmingly common argument, and `format_arg`'s `%s` arm eight hundred
  lines below ALREADY computes exactly that comparison
  (`class_id_by_name("java/lang/String") == class_id_of_object(obj)`) — so the
  fix is to hoist one lookup rather than add one. NOT done here because it is
  an optimization with no measurement behind it, and this lane does not run the
  binary. Anyone taking it should quote a before/after on a `%s`-dense
  benchmark, not a reasoned argument.

* **N5 — `Formatter.format`'s native shim reads the receiver's locale from
  FIELD 1 by index.** `native-builtins/src/lib.rs:21511` does
  `ctx.get_field(this, 1)` (and its synthetic-mode twin at `:42527` does the
  same — two registrars, one assumption) and its comment asserts that the real
  `java.util.Formatter`'s `l` is slot 1. That assertion is what the new
  `Formattable` path depends on: the `Formatter` handed to `formatTo` carries
  the call's locale in that slot, and a nested `f.format("%,.2f", x)` inside
  `formatTo` reads it back from there. If the real class's field order ever
  differs (a JDK update, a different image), the callee silently formats
  against the wrong locale with no refusal anywhere. The nomination is to
  resolve the field by NAME (`get_field_by_name(this, "l")`) with the index as
  the fallback, which is what `real_locale_language` already does for
  `baseLocale`/`language` one file over. Not this lane's file.

* **N6 — `%tc`'s zone is the fixed literal `UTC`.** Noticed while threading
  `uppercase` through `format_temporal_field`: the `'c'` composite writes
  `"UTC"` unconditionally, as do the `'z'`/`'Z'` field arms, because
  `extract_temporal_fields` models one zone. Pre-existing, documented in place,
  and out of scope here — but it means `%tc` and `%tZ` of a `Calendar` in any
  other zone are wrong in a way no `%t` row in any vector asks about.
  UNMEASURED against HotSpot on this lane.
