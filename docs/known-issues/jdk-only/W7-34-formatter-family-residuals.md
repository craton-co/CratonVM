# W7-34 — the twelve `java.util.Formatter` divergences that survived W7-3

**Status: source landed, UNVERIFIED against a VM.** Nothing here has been built
as part of the crate and `probes/ShadowDifferentialProbe.java` has **not** been
re-run. What *is* measured is stated as measured; what is computed is stated as
computed. The distinction matters more than usual in this record, because the
defect it corrects in its own predecessor is exactly the failure to draw it.

Everything below is in `native-builtins/src/lang_string.rs` unless a section
says otherwise.

## The measurement

`probes/ShadowDifferentialProbe.java` against HotSpot 25.0.3.9 and CratonVM
`--real-jdk` on one binary, same class files, both pinned to
`-Dstdout.encoding=UTF-8 -Duser.language=en -Duser.country=US`. Reproduced
2026-08-12 from `docs/known-issues/jdk-only/W7-32-round-2-differential-run.md`,
identical in every row.

| observable | HotSpot 25 | CratonVM before |
|---|---|---|
| `format.floatRoundingHalfUp` | `0.3\|0.4\|1` | `0.3\|0.3\|1` |
| `format.formatterAppendable` | `k=007;1.01` | `k=007;1.00` |
| `format.bigDecimalPrecision` | `2.35\|1,234,567.89` | `0.00\|0.00` |
| `format.parenthesisedNegative` | `(5)\|5` | `-5\|5` |
| `format.hexAndOctal` | `FF\|0xff\|10\|010` | `FF\|ff\|10\|10` |
| `format.localeGermany` | `1.234,50` | `1,234.50` |
| `format.localeFrance` | `1␣234␣567` (`␣` = U+202F) | `1,234,567` |
| `format.unknownConversion` | `java.util.UnknownFormatConversionException: Conversion = 'q'` | `no-throw` |
| `format.missingArgument` | `java.util.MissingFormatArgumentException` | `no-throw` |
| `format.wrongArgumentType` | `java.util.IllegalFormatConversionException` | `no-throw` |
| `format.illegalFlagCombination` | `java.util.IllegalFormatFlagsException` | `no-throw` |
| `format.precisionOnInteger` | `java.util.IllegalFormatPrecisionException` | `no-throw` |

## Two of the twelve were the same defect, and it was not the one W7-3 predicted

`format.floatRoundingHalfUp` is `%.1f|%.1f|%.0f` of `0.25, 0.35, 0.5` and only
the middle cell moved. `format.formatterAppendable` writes `%.2f` of `1.005`
through a `Formatter(Appendable)` and only that number moved — the
write-through to the supplied `StringBuilder` worked, so **`formatterAppendable`
was never an Appendable defect at all.** Both cells are one root cause.

`docs/known-issues/jdk-only/W7-3-format-conversions-and-stringbuilder-bounds.md`
predicted a residual in this exact code and named the wrong cause:

> Beyond roughly **20 significant digits** this implementation is *more* exact
> than HotSpot […] `FloatingDecimal.BinaryToASCIIBuffer` holds its digits in a
> `char[20]`.

It also said, correctly, that the 20-digit cap "was **not** confirmed against
JDK 25". It is not the cap. The javadoc for `'f'`, `'e'` and `'g'` each carry
the same sentence, and it names a different source entirely:

> If the precision is less than the number of digits which would appear after
> the decimal point in the string returned by `Double#toString(double)`, then
> the value will be rounded using the round half up algorithm. Otherwise, zeros
> may be appended to reach the precision.

The digits to round are `Double.toString`'s **shortest round-trip** digits. So
the divergence does not begin in a rare tail past 20 digits; it begins at the
first digit past the shortest representation, which is why it shows up at
`%.1f` of `0.35`. Measured on HotSpot 25:

| expression | HotSpot 25 | exact-expansion (W7-3) |
|---|---|---|
| `%.1f` of `0.35` | `0.4` | `0.3` (exact is 0.34999999999999997…) |
| `%.2f` of `1.005` | `1.01` | `1.00` (exact is 1.00499999999999989…) |
| `%.2f` of `9.995` | `10.00` | `9.99` |
| `%.17f` of `0.1` | `0.10000000000000000` | `0.10000000000000001` |
| `%.20f` of `0.1` | `0.10000000000000000000` | `0.10000000000000000555` |
| `%.3f` of `1.2345678901234569e23` | `123456789012345690000000.000` | `123456789012345685803008.000` |
| `%f` of `1e23` | `100000000000000000000000.000000` | `99999999999999991611392.000000` |

The exact expansion is the *better* number and the *wrong* answer. A caller who
asks `%.2f` of `1.005` and is told `1.00` disagrees with every other Java
runtime.

`fmt_exact_fraction_digits` and `fmt_exact_decimal` are therefore replaced by
`fmt_decimal_digits` — a parser for a Java number string — and
`fmt_shortest_decimal`, which feeds it `format_double`
(`cratonvm_types::java_double_to_string`). That function was measured
byte-identical to HotSpot 25's `Double.toString` on all sixteen values above
plus `BigDecimal.toPlainString` on the probe's two literals, so this reads a
real value rather than reproducing one. The renderers already zero-filled past
the digits they were given, so nothing downstream changed.

**So: `format.floatRoundingHalfUp` and `format.formatterAppendable` are the
residual W7-3 flagged, in the code W7-3 flagged — but its stated boundary was
wrong, and the two rows are the proof.** `format.bigDecimalPrecision` is not
that residual; see below.

### A second W7-3 row that only the javadoc had ever checked

W7-3 lists `%e` of `Double.MIN_VALUE` -> `4.940656e-324` among its verified
outputs. HotSpot 25 answers **`4.900000e-324`**: `Double.toString(4.9e-324)` is
`"4.9E-324"`, and `%e` rounds from those two digits and zero-fills. The
corrected digit source produces it without a special case. W7-3's 37 cases were
checked against the javadoc, not a runtime, and it says so — this is what that
costs.

## `format.bigDecimalPrecision` — a slot-0 read of a class that has no value in slot 0

`%.2f` of `new BigDecimal("2.345")` answered `0.00`. `extract_float_value` fell
through to `ctx.get_field(obj, 0)` on anything it did not recognise;
`BigDecimal`'s slot 0 is not its value, so the answer was `0.0`, formatted
faithfully. This is the same shape as the `BigInteger`-signum bug already
recorded in `format_arg`'s comments, one class along.

`java.util.Formatter.print(Object, Locale)` dispatches the float family to two
different printers, and `print(BigDecimal, Locale)` never converts to a double.
That is load-bearing rather than tidy: rounding `2.345` HALF_UP gives `2.35`,
while the nearest double is 2.34499999999999975… and gives `2.34`. So
`FloatSource` distinguishes the two, and a BigDecimal reaches the same
`%f`/`%e`/`%g` assembly through its own `toPlainString()`.

`%a` is the one float conversion with no `BigDecimal` printer in the JDK at all;
it raises `IllegalFormatConversionException`, and now does here.

## The five refusals

A no-throw is the campaign's dominant defect species. A *mistyped* throw is
worse in one specific way: `java.util.Formatter` specifies a distinct
`IllegalFormatException` subclass per failure and callers catch the subclass, so
a generic refusal walks past the `catch` that was written for it.

Each fault is raised as the class the javadoc names, constructed through that
class's **real `<init>`**:

| specifier | exception | specifying sentence |
|---|---|---|
| `%q` | `UnknownFormatConversionException` | "If the conversion is not one of the conversions defined above, an `UnknownFormatConversionException` is thrown." |
| `%s %s` with one argument | `MissingFormatArgumentException` | "If there are fewer arguments than format specifiers, the argument index is out of range […] a `MissingFormatArgumentException` is thrown." |
| `%d` of a `String` | `IllegalFormatConversionException` | "If the argument `arg` is `null` […] Otherwise, if `arg` is not […] applicable to this conversion, then an `IllegalFormatConversionException` will be thrown." |
| `%-08d` | `IllegalFormatFlagsException` | "If the `'-'` and `'0'` flags are both given […] an illegal combination of flags […] an `IllegalFormatFlagsException` will be thrown." |
| `%.2d` | `IllegalFormatPrecisionException` | "If a precision is provided then an `IllegalFormatPrecisionException` will be thrown." |

Not by allocating a Throwable and stuffing a message into slot 0. **All of them
override `getMessage()` off their own fields and never set
`Throwable.detailMessage`** — the same trap that kept
`java/util/regex/PatternSyntaxException` out of the synthetic-exception bridge
list in `native-builtins/src/lib.rs`, documented there since 2026-08-05. An
object built the other way would carry the right class and a null message.

`fmt_raise` falls back to `IllegalArgumentException` only when the specified
class is genuinely absent — a `--synthetic-jdk` build with no
`java.util.Formatter` exception hierarchy. That is the documented **superclass**
of `IllegalFormatException`, so it can never send a caller down a `catch` branch
it did not ask for. It is not there to make a diff go away.

### The neighbouring rules came with them

`checkNumeric`, `checkGeneral`, `checkInteger`, `checkFloat` and `checkText` are
five functions, not five rules. Implementing half of one leaves a validator that
the next lane has to re-derive, so the rest of each is here:

* `FormatFlagsConversionMismatchException` — `%,x`, `%,e`, `%#d`, `%#g`, `%#s`,
  `%(x`, `%(a`, `%+s`. Two of these are what `format.hexAndOctal` and
  `format.parenthesisedNegative` needed anyway.
* `MissingFormatWidthException` — `%-d`, `%0d`, `%-s`. "If the `'-'` or `'0'`
  flags are given, then the width is required."
* `DuplicateFormatFlagsException` — `%--8d`, checked during flag parsing before
  any conversion character is known, as `Flags.parse` does.
* `IllegalFormatWidthException` — `%5n`.

**Check order is load-bearing and is the JDK's.** `%#-s` reports the missing
width; `%#-8s` reports the flag mismatch. `%-08d` reports the illegal flag
combination rather than a missing width, because the `8` *is* the width and the
`0` is a flag.

Two message details that only a sweep finds:

* `FormatFlagsConversionMismatchException` names the **lower-case** conversion:
  `%,X` says `Conversion = x`. `Conversion.isValid` folds every upper-case
  conversion down and keeps the case in an internal `UPPERCASE` flag.
* `IllegalFormatFlagsException` names that internal flag, as `'^'`, between
  `'-'` and `'#'`: `%+ X` says `Flags = '^+ '` where `%+ x` says `Flags = '+ '`.
  `FormatSpecifier.toString` removes it again, so the `MissingFormatWidth`
  message does **not** carry it. The two disagree on purpose.

## `(` and `#`

`%(d` of `-5` is `(5)`: "the result will enclose negative numbers in
parentheses". The `'-'` disappears rather than sitting alongside. `%#x`/`%#X`
carry the radix indicator and `%#o` a leading `0`.

Both flags were parsed and then dropped. Restoring them exposed a third defect
in the width padder. The alternate prefix goes on **before** the zero padding
(`%#010x` is `0x000000ff`, not `00000000xff`) and the parentheses likewise
(`%(08d` is `(000005)`), so the zero-pad insert is now prefix-aware — and its
"ends with an ASCII digit" finiteness test, a stand-in for Formatter's
finite-branch structure, read a **hex** digit and a **closing parenthesis** as
infinities the moment those two tails existed. It is now the explicit
`Infinity`/`NaN` test it was standing in for.

The `' '` flag ("include a leading space for positive values") was silently
ignored in the same block and is implemented alongside, since the sign logic is
one function.

## Locale

`native_string_format_locale` **discarded its `Locale` argument** and delegated
to the no-locale path.

The separators are not guessable and were not guessed. `java.util.Formatter`'s
own `getZero`/`getDecimalSeparator`/`getGroupingSeparator` read three characters
off `DecimalFormatSymbols.getInstance(locale)`, so this does too. All four rows
are HotSpot 25; the first three were also run on CratonVM `--real-jdk`, which
answered the identical characters — so this reads a real value rather than
reproducing one, and the reading was checked before the code depended on it:

| locale | grouping | decimal | zero | also measured on CratonVM |
|---|---|---|---|---|
| ROOT, `en_US` | U+002C | U+002E | U+0030 | yes |
| `de_DE` | U+002E | U+002C | U+0030 | yes |
| `fr_FR` | **U+202F** narrow no-break space | U+002C | U+0030 | yes |
| `ar_EG` | U+066C | U+066B | U+0660 | no |

France's separator is not an ASCII space. Asserting "it looks French" would have
passed on U+0020 and been wrong.

Three implementation constraints, each with a reason:

* **Lazy.** Symbols are resolved at the first conversion that actually
  localizes, so `String.format(Locale.ROOT, "%s", x)` pays nothing.
* **Re-entrancy latch.** Resolving runs real JDK bytecode — resource bundles,
  locale providers — which is free to call `String.format(Locale, …)` itself and
  re-enter this native asking for symbols on a locale still mid-construction.
  While the latch is set the inner call takes the root defaults, which is the
  same answer the JDK's own null-locale branch gives and cannot recurse.
* **Applied last.** Localization runs after grouping, signs and width padding,
  so every length decision upstream is made on ASCII. U+202F is three UTF-8
  bytes; a byte-length pad computed over it would be short by two. The
  substitution is one character for one, so the padded character count survives.

Grouping is always by three. `Formatter` does not consult `DecimalFormat`'s
grouping size — `%,d` of 1234567 in `hi-IN` is `1,234,567`, not `12,34,567`.

## How this was verified, given that nothing was built

`cargo build` was not run (deliberately, per this lane's constraints), so the VM
has not executed a line of it. Three things were done instead.

1. **Parse check.** `rustfmt --emit stdout` on a *copy* of the edited file. The
   tree is not fmt-clean and nothing was reformatted in place.
2. **The pure functions, extracted verbatim and run.** A script pulls
   `fmt_decimal_digits`, `fmt_shortest_decimal`, `fmt_round_significant`,
   `fmt_round_at_fraction`, `fmt_render_fixed`, `fmt_render_scientific`,
   `fmt_hex_digits`, `fmt_hex_float`, `fmt_hex_pad`, `java_float_conversion`,
   `java_decimal_conversion`, `fmt_decimal_conversion`, `group_thousands`,
   `fmt_localize`, `fmt_flags_string`, `fmt_spec_text` and `fmt_check_spec` out
   of `lang_string.rs` by brace matching — no retyping, so the thing tested is
   the thing that landed — compiles them with `rustc --edition 2021`, and adds a
   copy of `format_arg_full`'s post-`raw` flag pipeline.
3. **Two sweeps against HotSpot 25, not against the javadoc.** A Java program
   prints the oracle's answer for every case; the Rust harness prints its own;
   the two are diffed.
   * **1763 cases** — 43 specifiers (`%f %e %g %a` at ten precisions, plus
     width/justify/zero-pad/`+`/space/`(`/`,` combinations) × 41 doubles
     (ties, subnormals, `MAX_VALUE`, `1e23`, `-0.0`, NaN, both infinities).
   * **8032 cases** — every subset of `- # + ␣ 0 , (` × 17 conversions ×
     {no width, 8} × {no precision, `.2`}, compared on exception **class name
     and message**.

   Both are at **0 failures**. They were not at zero when first run: the 8032
   found the two message details above, and the 1763 found `%.4a`.

### `%.4a`, which the sweep found and no hand-picked case would have

W7-3 argued from the javadoc that `%a` is not padded to its precision —
"nothing pads the digits back out, and `%.4a` of 1.0 is `0x1.0p0`, not
`0x1.0000p0`" — because `hexDouble` rounds in binary and re-renders through
`Double.toHexString`. HotSpot 25 answers `0x1.0000p0`. The padding is real; it
lives *outside* `hexDouble`, as `if (prec != 0) addZeros(va, prec)`. It applies
above 13 hex digits too, where `hexDouble` stops rounding but `addZeros` keeps
padding: `%.14a` of `Double.MIN_VALUE` is `0x0.00000000000010p-1022`.
`fmt_hex_pad` is that step.

The probe's `format.hexFloat` is `%a` of 1.0 with no precision, which matched
before and after. This defect was invisible to the probe, to W7-3's 37 cases,
and to my own first 100 assertions. Only enumerating the precisions found it.

## In-tree callers

Grepped `regression-suite/`, `probes/`, `apps/` and `test_classes/` for Java,
and the whole tree for Rust:

* `%(` and `%#x`/`%#o`: **only** `probes/ShadowDifferentialProbe.java`.
* `String.format(Locale, …)` with a locale other than ROOT/US/ENGLISH: **only**
  the same probe.
* `%-d` / `%-s` without a width, which now throws `MissingFormatWidthException`:
  **none**.
* Rust callers all reach `lang_string::native_string_format` (the no-locale
  overload) with an unchanged signature: `native_printf`,
  `native_formatter_format` and the `java/util/Formatter.format` registrations
  in `native-builtins/src/lib.rs`, and `logging_shims.rs`'s `PrintWriter.format`.
  `format_arg_full` and `format_arg` have no callers outside this file.

Nothing in the tree depends on the old behaviour. The changes are
`Compatible`-mode by construction — every one is a wrong value or an absent
refusal becoming the specified one — but `%q`, `%-d` and `%.2d` now throw where
they used to pass, and any caller that was relying on a typo being echoed back
will see it.

## Out-of-file patch (not applied)

### `native-builtins/src/lib.rs` — `new Formatter(…, Locale)` still drops its locale

`String.format(Locale, …)` now honours its locale; a `Formatter` constructed
with one does not, because `Formatter.format(String, Object[])` has no locale
argument to carry — it has to come off the receiver. There are **two**
registrars for `java/util/Formatter` and neither does it. This is not one of the
twelve (the probe uses `Locale.ROOT`, whose symbols are the defaults, so
`format.formatterAppendable` cannot see it), but it is the same defect one level
up and it is measurable: `new Formatter(sb, Locale.GERMANY).format("%,.2f",
1234.5)` gives `1,234.50` where HotSpot gives `1.234,50`.

**Registrar 1** (`let f = "java/util/Formatter";`, ~line 20992). The
`(Ljava/util/Locale;)V` constructor here *already* stores the locale in field 1;
`format` just never reads it. Inside the `"format"` registration, replace:

```rust
            // Delegate to the full String.format implementation
            let format_result = lang_string::native_string_format(ctx, &[fmt_obj, arr_obj])?;
```

with:

```rust
            // Field 1 is the Locale this Formatter was constructed with (see the
            // `(Ljava/util/Locale;)V` ctor above). `Formatter.format(String,
            // Object[])` has no locale argument, so the receiver's is the only
            // place it can come from — and dropping it made
            // `new Formatter(sb, Locale.GERMANY).format("%,.2f", 1234.5)`
            // answer the US `1,234.50`.
            let locale = ctx.get_field(this, 1);
            let format_result =
                lang_string::native_string_format_locale(ctx, &[locale, fmt_obj, arr_obj])?;
```

and add the missing two-argument constructor beside the other three:

```rust
    registry.register(
        f,
        "<init>",
        "(Ljava/lang/Appendable;Ljava/util/Locale;)V",
        |ctx, args| {
            let this = match args.first() {
                Some(Value::Object(Some(o))) => *o,
                _ => return Ok(None),
            };
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            ctx.set_field(this, 1, args.get(2).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
```

**Registrar 2** (`let c = "java/util/Formatter";`, ~line 40625). Here
`native_formatter_init` serves both `()V` and `(Ljava/util/Locale;)V` and writes
`null` into field 1 either way, so the locale is dropped at construction. Split
them:

```rust
fn native_formatter_init_locale(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = match args.first() {
        Some(Value::Object(Some(o))) => *o,
        _ => return Ok(None),
    };
    let empty = ctx.create_string("");
    ctx.set_field(this, 0, Value::Object(Some(empty)));
    ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Object(None)));
    Ok(None)
}
```

register it for `(Ljava/util/Locale;)V` (and an equivalent for
`(Ljava/lang/Appendable;Ljava/util/Locale;)V`, which neither registrar declares
at all), and make `native_formatter_format` read field 1 exactly as above.

Note for whoever applies this: two registrars for one class means the **last one
registered wins**. Patching only one is indistinguishable from patching none —
diff `--dump-native-registry` before and after.

## What is left

* **`%t`/`%T`** is untouched. It has no `checkDateTime` (so `%.2tY` does not
  raise `IllegalFormatPrecisionException`) and an out-of-range argument index
  still emits nothing instead of `MissingFormatArgumentException`. A truncated
  `%t` at the end of a format string *was* corrected, since it shares the
  specifier-parse refusal.
* **`%h`/`%H`** returns the argument's *string* rather than
  `Integer.toHexString(arg.hashCode())`. HotSpot 25: `%H` of `"a"` is `61`.
  Not probed, so not measured beyond that one case.
* **`%012a`** — the `'0'` flag on a hex float. Formatter puts the zeros *after*
  the `0x` prefix (`%020a` of 1.0 is `0x00000000000001.0p0`); the generic
  padder still excludes `a`/`A`, as it did before. Left with its existing
  argued comment.
* **`String.format(String, Object[])`** (no locale overload) still formats with
  the root separators rather than `Locale.getDefault(Locale.Category.FORMAT)`.
  Invisible under the probe's `-Duser.language=en -Duser.country=US`, and
  resolving a default locale on the no-locale path is the one place where the
  re-entrancy hazard is worst — every internal `String.format` in the VM,
  including the logging shims, goes through it. Deliberately left.
* W7-3 left `append(CharSequence,int,int)` clamping and `appendCodePoint`
  truncating, each with an argued rationale and a pinning unit test. **Not
  touched, and not disagreed with** — neither is in this family.
* Nothing here has run on a VM. The next lane with a build should re-run
  `probes/ShadowDifferentialProbe.java` and diff the `format.` rows.
