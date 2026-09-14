# W7-34 — the twelve `java.util.Formatter` divergences that survived W7-3

> ## 2026-08-12 (P3-E) — the no-`Locale` overload now takes the FORMAT default. The `%t` "hard-coded English tables" paragraph below is STALE. And one lib.rs surface must land WITH this or an existing green vector goes red
>
> **Discharged.** §"What is left" bullet 4 — *"`String.format(String, Object[])`
> (no locale overload) still formats with the root separators rather than
> `Locale.getDefault(Locale.Category.FORMAT)` … Deliberately left."* — is
> **CLOSED** in `native-builtins/src/lang_string.rs`. W7-91 §5 is the same row.
>
> **What the fix is.** `format_impl`'s `locale` parameter was
> `Option<ObjectRef>`, and `None` meant two different requests at once:
>
> | request | JDK's rule | old `None` behaviour |
> |---|---|---|
> | overload has **no** `Locale` (`String.format(String, Object...)`, `String.formatted`, `PrintStream.printf(String, …)`) | `Locale.getDefault(Locale.Category.FORMAT)` | root separators — **wrong** |
> | overload given an explicit **`null`** | "no localization is applied" | root separators — correct |
>
> It is now a three-armed `FmtLocale { DefaultFormat, Given(Option<..>) }`,
> reconciled **at consumption** in `fmt_symbols_for` and `fmt_date_name` rather
> than by re-encoding one arm as the other. `DefaultFormat` resolves through the
> **no-arg** `DecimalFormatSymbols.getInstance()`, whose body already *is*
> `getInstance(Locale.getDefault(Locale.Category.FORMAT))` — the same "ask the
> JDK rather than compose a fourth opinion" route `fmt_date_name` was already
> taking.
>
> **The two helpers were drifted twins, and that is how this was found.** One
> JDK rule — "an absent `Locale` means the FORMAT default" — implemented twice
> in one file: `fmt_date_name`'s absent-locale arm had been correct since the
> `%t` name fields landed (it called the no-arg `DateFormatSymbols.getInstance()`),
> while `fmt_symbols_for`'s took the root constants. `RStrings` even asserts the
> correct half (`String.format("%tb", …)` vs `DateFormatSymbols.getInstance()`),
> so the date side was *covered* while the number side was wrong beside it.
>
> **The re-entrancy hazard this record named as the reason to leave it is
> bounded by two mechanisms that were already there**, not by new code:
> *laziness* (`format_impl` calls `fmt_symbols_for` only at a
> `%d`/`%f`/`%e`/`%g` conversion, so an internal `String.format` over `%s` runs
> zero locale bytecode) and `FMT_SYMBOLS_RESOLVING` (an inner call answers the
> root constants and cannot recurse). `fmt_date_name` has been taking exactly
> this route on exactly this path already.
>
> **The `Locale.ROOT` fast path is untouched.** The only fast path ROOT ever had
> is that laziness; `Given(Some(l))` resolves exactly as before. Nothing was
> added to any explicit-`Locale` path.
>
> ### BLOCKING co-requisite in `native-builtins/src/lib.rs` (NOT this lane's file)
>
> `native_printf_locale` **drops its `Locale`** and delegates to
> `native_printf` → `native_string_format`. Its own doc comment justifies that
> with *"exactly as `String.format(Locale, …)` already drops it"* — a claim this
> record's own 2026-08-12 patch made **false**, and which is now actively
> harmful: with the no-`Locale` overload following the FORMAT default,
> `System.out.printf(Locale.ROOT, …)` renders the **host default** instead of
> ROOT. `regression-suite/src/RJdkHello.java` asserts
> `ps.printf(Locale.ROOT, " [%s|%d|%05.2f]", …)` equals `" [x|7|01.50]"`
> character for character, so on any host whose FORMAT default is not
> ROOT/en-US that vector goes **RED**. It stays green on an en-US CI box, which
> is the same hiding place the defect being fixed used. The patch is in the
> P3-E lane report; it is two lines and must land in the same change.
>
> The same file's `Formatter` `()V` and `(Ljava/lang/Appendable;)V` constructors
> write a **null** locale into slot 1 where the real `java.util.Formatter`
> constructors write `Locale.getDefault(Locale.Category.FORMAT)`. That is the
> `new Formatter()` half of this row and it is **still open** — see §"What is
> left". It is not a regression from this change (a null slot 1 meant root
> separators before and means root separators now), and it is why
> `fmt_date_name` deliberately does **not** implement `printDateTime`'s
> `Locale lt = ((l == null) ? Locale.US : l)`: `Given(None)` in this VM is both
> "explicit null" and "`new Formatter()`", and answering English would trade a
> rare divergence for a common regression. The function's doc comment says so.
>
> ### `%t`'s "hard-coded English tables" — STALE since W7-91
>
> The paragraph below headed *"Also from 'What is left', now measured against
> the source rather than assumed"* says `%tB`/`%tb`/`%tA`/`%ta`/`%tp` are
> answered from `MONTHS_ABBR` / `MONTHS_FULL` / `DAYS_ABBR` / `DAYS_FULL`
> "consulted for every locale". **They are not.** Those four arrays are now the
> FALLBACK for a configuration with no `DateFormatSymbols` to ask; the live path
> is `fmt_date_name` → `java.text.DateFormatSymbols`, and `RStrings` asserts
> each of the six name fields against the JDK's own table. Read that paragraph
> as history.
>
> ### Coverage
>
> `regression-suite/src/RJdkFormatLocale.java` (new; **needs registering in
> `CORE_CLASSES`** — see the lane report). It **pins the FORMAT default itself**
> to `Locale.GERMANY` and restores it in a `finally`, so it asserts the RULE
> rather than the host's incidental locale — a vector that merely read the
> host's default would be green on en-US against a VM that always answered ROOT.
> Every expectation is an exact string equality; where the expected text depends
> on locale DATA it is built from that locale's own `DecimalFormatSymbols`, so a
> platform without German data cannot manufacture a red, and the CK line reports
> whether the implication had an antecedent. It covers `String.format`,
> `String.formatted`, `PrintStream.printf`, `PrintStream.format`, the
> `printf(Locale.ROOT, …)` anti-overshoot, the explicit-`null` arm, and the
> `%tB`/`%tb` date twin. **Measured green on HotSpot 25.0.3.9, 20 checks,
> byte-identical CK lines under `-Duser.language=` en-US / ru-RU / fr-FR /
> ar-EG.** Mutation-checked: five separate injected regressions (no-locale
> numeric → ROOT, `formatted` → ROOT, `printf` → ROOT, `printf(Locale.ROOT)`
> dropping its locale, `%tB` → ROOT) each fail it, at the intended assertion.
> **Not run on CratonVM** — this lane does not build.

> ## 2026-08-12 — the locale patch is APPLIED, and the registrar order this record said had to be settled first IS settled
>
> **Which registrar wins, read from the boot path rather than from a census.**
> Both live in `native-builtins/src/lib.rs`, and they are not peers:
>
> | registrar | enclosing fn | reached from | ambient kind | registers under `--jdk-only` / `--real-jdk`? |
> |---|---|---|---|---|
> | 1 — `let f = "java/util/Formatter";` (~`lib.rs:21308`) | `register_string_format_real_jdk_natives` (`lib.rs:21270`) | `register_essential_natives_with_shims` at `lib.rs:19292` | `Intrinsic` (set at `lib.rs:21274`, restored at `lib.rs:21434`) | **yes — and it is the only one** |
> | 2 — `let c = "java/util/Formatter";` (~`lib.rs:41407`) | `register_formatter_natives` (`lib.rs:41402`) | `register_enterprise_final_natives` (`lib.rs:41149`) ← `register_synthetic_overrides` (`lib.rs:23897`), which is `#[cfg(feature = "synthetic-jdk")]` and called only from `register_builtins` on the `use_synthetic_jdk` arm | `Intrinsic` (set at `lib.rs:41406`) | **no** |
>
> So this record's own closing note — *"two registrars for one class means the
> last one registered wins; patching only one is indistinguishable from patching
> none"* — is **true only in `--synthetic-jdk`**. On the two shipping modes
> registrar 2 never registers at all, so registrar 1 wins by default rather than
> by ordering, and patching it is necessary AND sufficient. This is
> docs/architecture/natives-over-real-jdk-classes.md §3's second shape ("a
> shadowing verdict that ignores this is backwards"), and it is why the patch
> could land without the `--dump-native-registry` diff the note demanded.
> Registrar 2 is patched too, so synthetic mode agrees.
>
> **What landed.**
>
> * Registrar 1's `format` now reads the receiver's `Locale` out of field 1 and
>   calls `lang_string::native_string_format_locale`. A null field 1 means "root
>   defaults", which is exactly what `native_string_format` did unconditionally,
>   so the no-locale path is byte-for-byte unchanged.
> * Registrar 2's `native_formatter_format` does the same, and its
>   `native_formatter_init` — which served `()V` **and** `(Locale)V` and wrote
>   null into field 1 either way — is split, with `native_formatter_init_locale`
>   and a new `native_formatter_init_appendable_locale`.
>
> **What was deliberately NOT taken from §"Out-of-file patch": the
> `(Ljava/lang/Appendable;Ljava/util/Locale;)V` constructor on REGISTRAR 1.**
> The patch proposed adding it there. On registrar 1's boot path the class is
> the real `java.util.Formatter`, whose own constructor already writes `a` at
> slot 0 and `l` at slot 1 — the two slots this layout uses — **plus `zero`**,
> the digit-base field no native writes and real `Formatter` bytecode reads (the
> `format(Locale, String, Object[])` overload has no native). Shadowing that
> constructor would have traded a locale bug for an unwritten `zero`. Once
> `format` reads slot 1, the real constructor is all registrar 1 needs. Registrar
> 2 *does* need the overload — there is no real constructor in synthetic mode —
> and has it. This is the "a fix that only pins the positive half" rule run
> forwards: the constructor and the reader are one state machine, and on one arm
> the constructor half is already correct.
>
> **Coverage.** `regression-suite/src/RStrings.java` (`CORE_CLASSES`, so it runs
> in a plain `bash run.sh` and again under `CRATONVM_ARGS=--jdk-only`) asserts
> that `new Formatter(sb, Locale.GERMANY).format("%,.2f", 1234.5)` equals
> `String.format(Locale.GERMANY, "%,.2f", 1234.5)` — an equality between the two
> spellings of one request rather than a pinned `1.234,50`, so a platform whose
> German locale data is unavailable cannot turn it into a false red — and, as an
> implication guarded on `DecimalFormatSymbols.getInstance(Locale.GERMANY)`
> actually reporting `,`/`.`, that the rendering then uses them. The implication
> is the locale-SENSITIVE half: a VM that resolves the symbols and discards the
> locale fails it, which a `contains`-over-English check cannot see.
>
> **Also from "What is left", now measured against the source rather than
> assumed:** `%t`/`%T` is not merely missing its `checkDateTime` — its month,
> weekday and AM/PM renderings are **hard-coded English tables**
> (`MONTHS_ABBR`, `MONTHS_FULL`, `DAYS_ABBR`, `DAYS_FULL` and the `'r'`/`'R'`
> AM-PM arm in `native-builtins/src/lang_string.rs`'s date-time conversion),
> consulted for every locale. `java.util.logging.SimpleFormatter`'s default
> pattern opens with `%1$tb %1$td, %1$tY … %1$Tp`, so every JUL console line
> renders its date in English whatever `Locale.getDefault(Locale.Category.FORMAT)`
> says. Not fixed here (it needs `DateFormatSymbols`, and the re-entrancy latch
> this record documents for `DecimalFormatSymbols`), and **not asserted** in
> `RJdkLogging` — an assertion there would be red for a defect that row does not
> gate, and would be date-dependent besides. A comment at that check says so.

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

  > **RE-GREPPED 2026-08-12 (P3-E), and this list is now wrong in two places.**
  > The `java/util/Formatter.format` registrations reach
  > `native_string_format_**locale**` (both registrars, since this record's own
  > patch), not the no-locale entry. The current callers of the no-locale entry
  > are exactly three: `native_printf` (`lib.rs`, which
  > `native_printf_locale` also funnels into — see the head of this record),
  > `native_printwriter_printf` (`logging_shims.rs`), and
  > `native_string_formatted` (same file). All three are surfaces the JDK
  > specifies as taking the FORMAT default, so all three moved together with the
  > P3-E fix and none of them needed a signature change. `native_string_format`'s
  > signature is still unchanged; the `FmtLocale` threading is entirely below it.

Nothing in the tree depends on the old behaviour. The changes are
`Compatible`-mode by construction — every one is a wrong value or an absent
refusal becoming the specified one — but `%q`, `%-d` and `%.2d` now throw where
they used to pass, and any caller that was relying on a typo being echoed back
will see it.

## Out-of-file patch (APPLIED 2026-08-12, with one deliberate departure)

> Applied as written for the two `format` bodies and for registrar 2's
> constructor split. **Registrar 1's `(Ljava/lang/Appendable;Ljava/util/Locale;)V`
> constructor was NOT added** — see the head of this record for why (the real
> constructor already writes slots 0 and 1, and also `zero`, which a native
> would not). The registrar-order question the closing note raises is settled
> at the head of this record: registrar 2 is synthetic-only, so on the shipping
> modes registrar 1 wins by default and a `--dump-native-registry` diff is not
> the instrument that settles it — the call graph is.

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
* ~~**`String.format(String, Object[])`** (no locale overload) still formats with
  the root separators rather than `Locale.getDefault(Locale.Category.FORMAT)`.
  Invisible under the probe's `-Duser.language=en -Duser.country=US`, and
  resolving a default locale on the no-locale path is the one place where the
  re-entrancy hazard is worst — every internal `String.format` in the VM,
  including the logging shims, goes through it. Deliberately left.~~
  **CLOSED 2026-08-12 (P3-E)** — see the head of this record. The re-entrancy
  argument was right about the hazard and wrong about the mitigation: laziness
  and `FMT_SYMBOLS_RESOLVING` already bound it, and `fmt_date_name` was already
  running the identical route on the identical path.
* **`new Formatter()` and `new Formatter(Appendable)` still format with the root
  separators**, where the real `java.util.Formatter` constructors carry
  `Locale.getDefault(Locale.Category.FORMAT)`. This is the last piece of the
  locale row and it is in `native-builtins/src/lib.rs`, not `lang_string.rs`:
  registrar 1's `<init>()V` and `<init>(Ljava/lang/Appendable;)V` natives write
  a **null** into slot 1, and `format` correctly reads a null slot 1 as "no
  localization". Two shapes of fix, and the choice is not obvious — (a) write
  the FORMAT default into slot 1 in both constructors, or (b) stop registering
  them and let the real constructors run, which is the argument this record
  already accepted for `(Appendable, Locale)` (they write slot 0, slot 1 **and**
  `zero`) but which changes slot 0 from a `String` to a `StringBuilder` for the
  `()V` case and needs `format`'s append path and `Formatter.toString` checked
  against that. Whoever takes it: (b) is the one that cannot drift, and it needs
  a build to land. Not asserted in `RJdkFormatLocale`, deliberately, with a
  comment at the site saying why.
* **`PrintStream.printf/format(Locale, …)` drops its `Locale`**
  (`native_printf_locale`, `native-builtins/src/lib.rs`). See the BLOCKING note
  at the head of this record — after the P3-E fix this is no longer a quiet
  wrong answer, it breaks `RJdkHello` on any non-ROOT-default host.
  `PrintWriter.printf/format(Locale, …)` is not registered at all and falls
  through to real bytecode; unmeasured.
* **`String.format((Locale) null, "%tB", d)`** renders the FORMAT default's
  month name where HotSpot's `printDateTime` renders `Locale.US`'s. Kept on
  purpose while the `new Formatter()` row above is open — the two readings of
  `Given(None)` cannot both be served, and this is the rarer of the two. Fix it
  in the same change that fixes the constructors, not before.
* W7-3 left `append(CharSequence,int,int)` clamping and `appendCodePoint`
  truncating, each with an argued rationale and a pinning unit test. **Not
  touched, and not disagreed with** — neither is in this family.
* Nothing here has run on a VM. The next lane with a build should re-run
  `probes/ShadowDifferentialProbe.java` and diff the `format.` rows.
