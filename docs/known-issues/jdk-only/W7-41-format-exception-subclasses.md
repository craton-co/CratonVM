# W7-41 — `String.format` refused correctly and threw the wrong class

**Status: source landed, UNVERIFIED against a VM.** Nothing here has been built
and `probes/ShadowDifferentialProbe.java` has not been re-run. The measurement
this starts from is W7-40's, and is quoted as measured; everything downstream of
it — the root cause, the population count, the new refusals — is read from the
source of `native-builtins/src/lang_string.rs` and of JDK 25's own
`java.base/java/util/Formatter.java`, and is stated as such.

> **VERIFIED AGAINST A BINARY 2026-09-01.** The status above was written by a
> lane that could not build or run Rust, and it stood for roughly three weeks.
> Run on a release binary of `dev`, against HotSpot 25.0.4+7 on the same host:
>
> ```text
> RJdkFormatLocale
>   HotSpot          PASS RJdkFormatLocale (20 checks)
>   CratonVM compat  PASS RJdkFormatLocale (20 checks)      0 differing lines
>   CratonVM strict  PASS RJdkFormatLocale (20 checks)      0 differing lines
> ```
>
> Byte-identical output in BOTH modes, so the source work this record describes
> does what it claimed on a real binary.
>
> **The predicted COUNT is superseded: this record expected no count of its own; `RJdkFormatLocale` was named as the vector that exists.** Other
> lanes added to the shared vector across the three weeks. A count written as an
> expectation ages into a falsehood the moment a shared vector grows -- what
> survives verification is the ASSERTIONS, and those match. Do not re-derive a
> defect from a count that merely moved.


Everything below is in `native-builtins/src/lang_string.rs` unless said
otherwise.

> ## Re-verified 2026-08-12 (record-triage lane, doc-only — nothing built or run)
>
> A source read of today's tree. Nothing here upgrades the status line: this
> record is still **UNVERIFIED against a binary**, and this lane could not build
> either.
>
> **Present, at today's lines.** `fmt_exception_class_available` (`:5420`),
> `fmt_raise` (`:5467`), `FMT_DATETIME_FIELDS` (`:5526`), and both previously
> unraised classes — `java/util/IllegalFormatCodePointException` (`:5347`) and
> `java/util/IllegalFormatArgumentIndexException` (`:5358`). The `%c` gate is at
> `:7850-7862` and reads as this record describes it.
>
> **The evidence is still NOT SCHEDULED, and that is the residual to act on.** A
> sweep of `regression-suite/src/*.java` for the twelve class names finds **zero**
> hits, so no fixture asserts any of them; the only instrument is
> `probes/ShadowDifferentialProbe.java`, and `regression-suite/run.sh` never runs
> `probes/` at any `SUITE=` value. A green suite therefore says nothing about this
> record. What has changed since it was written is that a **scheduled host now
> exists**: `regression-suite/src/RJdkFormatLocale.java` is in `CORE_CLASSES`
> (`run.sh:106`) and is diffed against HotSpot. Every class here extends
> `IllegalArgumentException` and HotSpot 25 agrees on all of them, so the
> assertions are arm-uniform and safe to add there. That nomination is filed with
> this pass.
>
> **The two sibling changes landed and neither discharges anything here.**
> * The locale rework is in: `FmtLocale` with its `DefaultFormat` arm
>   (`:5765-5811`) and `fmt_symbols_for` (`:5827`), so the no-`Locale` overload
>   now follows `Locale.getDefault(Locale.Category.FORMAT)` instead of ROOT. That
>   moves *rendering*, not refusal: which fault a bad specifier raises is decided
>   before any symbol is read, and `FMT_DATETIME_FIELDS` is locale-independent.
>   The `%t` name-field half was already handed to `W7-91` by the addendum below.
> * The `defineClass` linkage retyping is in
>   (`native-builtins/src/lang_system.rs:4642`,
>   `classloading/src/class_manager.rs:5500`), so `UnsupportedClassVersionError`
>   and friends now arrive as their real JDK types. No residual in this record
>   touches `defineClass` or `ClassFormatError`; it is a neighbour, not a
>   dependency.
>
> **Two residuals re-read and still live, in source:** the lone-surrogate `?` is
> at `:7854-7861` and its comment names this record; the `%t` argument-type check
> is still class-name-keyed — `extract_temporal_fields` tests `Long`,
> `java/util/Date`, `java/util/Calendar` and a hand-listed `java/time` set
> (`:6641-6672`) with no `TemporalAccessor` interface test anywhere in the arm.

## The measurement, from W7-40-differential-at-14.md

HotSpot 25.0.3.9 vs CratonVM `--real-jdk`, same class files, both pinned to
UTF-8 / en-US.

| observable | HotSpot 25 | CratonVM |
|---|---|---|
| `format.unknownConversion` | `java.util.UnknownFormatConversionException: Conversion = 'q'` | `java.lang.IllegalArgumentException: Conversion = 'q'` |
| `format.missingArgument` | `java.util.MissingFormatArgumentException` | `java.lang.IllegalArgumentException` |
| `format.wrongArgumentType` | `java.util.IllegalFormatConversionException` | `java.lang.IllegalArgumentException` |
| `format.illegalFlagCombination` | `java.util.IllegalFormatFlagsException` | `java.lang.IllegalArgumentException` |
| `format.precisionOnInteger` | `java.util.IllegalFormatPrecisionException` | `java.lang.IllegalArgumentException` |

The refusals were right. All five extend `IllegalArgumentException`, which is
exactly why a probe that only asks "did it throw" cannot see this — and exactly
why a caller writing `catch (MissingFormatArgumentException e)` never enters the
block.

## The root cause: one predicate that could not be true

The per-fault modelling was already there. W7-34 landed a `FmtFault` enum
carrying nine faults, each with the class the javadoc names, each constructed
through that class's real `<init>` (every one of them overrides `getMessage()`
off its own fields, so allocating and stuffing a `detailMessage` gives an
object whose `getMessage()` is null). It also landed a fallback to the base
`IllegalArgumentException` for a build with no `java.util` exception hierarchy.

`fmt_raise` chose between them like this:

```rust
if ctx.class_id_by_name(class_name).is_some() {
    …construct the real subclass…
}
…fall back to IllegalArgumentException with a reconstructed message…
```

`class_id_by_name` is `find_unique_class_by_name` — an index read over the
**already-loaded** classes (`vm/src/vm/vm_exec.rs`). It loads nothing. And
nothing in a normal program has any reason to touch
`java.util.UnknownFormatConversionException` before the moment `String.format`
needs to throw one. So the predicate was false on every first refusal, the
fallback ran, and the caller got the base class carrying the message this file
reconstructs for the fallback's own use.

`format.unknownConversion=java.lang.IllegalArgumentException: Conversion = 'q'`
is that fallback's exact signature: the right message, from
`FmtFault::message`, in the wrong class. The five rows are not five defects.
They are one predicate, seen five times.

The species is worth naming: **a capability gated on a lookup that never
populates itself looks identical to a capability that was never written.** The
whole subclass machinery — enum, constructors, message table — was live code
behind a switch that was structurally off. Compare
`an-inert-registration-looks-exactly-like-a-missing-feature.md`.

### The fix

`fmt_exception_class_available` asks the class *loader*, not the index, and is
screened twice so the load cannot make things worse than the defect it repairs:

* `would_fabricate_synthetic_stub(name)` is checked **first**, and is
  non-destructive by construction — it answers "would a load of this name be
  met with a minted stub?" without minting one. A stub has no `<init>` and does
  not extend `IllegalArgumentException`; throwing it would convert a
  wrong-superclass defect into an object that no `catch
  (IllegalArgumentException)` can catch, which is strictly worse than the bug.
* after the load, `is_subclass(class_id, java/lang/IllegalArgumentException)`
  must hold. That is the invariant the fallback silently relies on, and
  confirming it costs one index read.

The `Err` from the speculative `load_class` is dropped on purpose: a
`ClassNotFoundException` from this lookup is not the answer `String.format`
owes its caller — the format refusal is — and CratonVM carries a thrown
exception in the return value rather than in thread state, so nothing is left
pending to clear.

A construction that fails *after* both screens is now also allowed to fall
back, where it previously propagated. An `InternalError` (heap exhaustion, a
broken class file) still propagates — that is a VM fault and must not be
dressed up as a format refusal. A thrown Java exception from the constructor
means the class passed the screens but its `<init>` refused, and in that case
the caller is owed the refusal it asked for, in the base class the subclass
would have extended.

## The population: 12, not 5

The five rows are one probe's coverage, not the population. Counting from
JDK 25's `java.base/java/util`, `IllegalFormatException` is `sealed` and
`permits` exactly twelve concrete subclasses:

| # | class | reachable from a format string? | before | after |
|---|---|---|---|---|
| 1 | `UnknownFormatConversionException` | yes | base class | correct |
| 2 | `MissingFormatArgumentException` | yes | base class | correct |
| 3 | `IllegalFormatConversionException` | yes | base class | correct |
| 4 | `IllegalFormatFlagsException` | yes | base class | correct |
| 5 | `IllegalFormatPrecisionException` | yes | base class | correct |
| 6 | `IllegalFormatWidthException` | yes | base class | correct |
| 7 | `MissingFormatWidthException` | yes | base class | correct |
| 8 | `DuplicateFormatFlagsException` | yes | base class | correct |
| 9 | `FormatFlagsConversionMismatchException` | yes | base class | correct |
| 10 | `IllegalFormatCodePointException` | yes | **not raised at all** | correct |
| 11 | `IllegalFormatArgumentIndexException` | yes | **not raised at all** | correct |
| 12 | `UnknownFormatFlagsException` | **no** — see below | n/a | n/a |

So: **eleven of the twelve are reachable and all eleven were wrong.** Nine were
wrong by class (the sampled five plus four the probe does not exercise); two
were not raised at all — the condition simply produced a wrong answer instead
of a refusal.

`FormatterClosedException` is sometimes counted with this family and is not part
of it: it extends `IllegalStateException`, not `IllegalFormatException`, and
belongs to `Formatter`'s lifecycle rather than to its format strings. It is
already registered in the exception-extras list in `native-builtins/src/lib.rs`.

### Why `UnknownFormatFlagsException` is excluded rather than missed

Its only throw site is the `default` arm of `Formatter.Flags.parse(char)`. The
only caller of `Flags.parse(String, int, int)` is
`FormatSpecifier.flags(s, start, end)`, over exactly the run of characters
`FormatSpecifierParser.parseFlag` accepted — and `parseFlag` loops on
`Flags.isFlag(c)`, whose `true` cases are precisely the eight characters
`Flags.parse(char)` recognises. The `default` arm is dead for every possible
format string; only a hand-built caller of the package-private `Flags` could
reach it.

CratonVM's scanner has the same shape (its flag loop is `"-+0 #(,<".contains`),
so the exclusion is symmetric rather than a gap. Modelling it would have added
a `FmtFault` variant no input can produce — a probe that cannot fail, in the
sense of INDEX_vacuous_greens. It is named in a comment on the enum so the next
reader does not have to re-derive the argument.

### `IllegalFormatArgumentIndexException` is package-private

JDK 25 declares it `final class`, not `public final class`, with a
package-private constructor. Only `java.util` code can name it. CratonVM's
native construction path does not apply that access check today, so it is
raised as itself, and `getClass().getName()` — which is what a differential
transcript records — reports the full name regardless. If this VM ever grows
the check, `fmt_raise`'s fallback catches the refusal and the fault degrades to
the base class rather than to a `NoSuchMethodError`. That degradation path is
the reason it was safe to ask.

## What else changed, and why it had to

Getting the two unraised faults right meant fixing the code paths that were
producing an answer where a refusal was owed. Each of these is a HotSpot-parity
correction read out of `Formatter.java`, not a judgement call.

**Scan and validate are now separate phases.** `java.util.Formatter` splits
them: `FormatSpecifierParser.parse` only *measures* the pieces and returns 0 for
a specifier it cannot complete, and the `FormatSpecifier` constructor is the
only thing that throws — in source order, index → flags → width → precision →
conversion. That split is what decides which refusal a doubly-illegal specifier
gets. `%0$s` is an illegal *index*; a bare `%0$`, with no conversion character
to complete it, is an unknown *conversion*. `%--d` is a duplicate flag; `%--`
is an unknown conversion. CratonVM raised the duplicate-flag fault during the
scan, so it won both times. The index and duplicate-flag faults are now
recorded and raised at the point the JDK's constructor would reach them.

**`%0$s` and an index that overflows `int`** → `IllegalFormatArgumentIndexException`,
messages `Illegal format argument index = 0` and (for the overflow, which the
JDK reports as `Integer.MIN_VALUE`) `Format argument index: (not representable
as int)`. Previously the index was silently discarded and the specifier
consumed the next ordinary argument.

**`%c` of a code point above `0x10FFFF`** → `IllegalFormatCodePointException`,
message `Code point = 0x110000` (`String.format("Code point = %#x", c)`, so a
negative code point renders as unsigned 32-bit hex). Previously
`char::from_u32(…).unwrap_or('?')` answered `?`.

**A width or precision that overflows `int`** → `IllegalFormatWidthException` /
`IllegalFormatPrecisionException` carrying `Integer.MIN_VALUE`, which is the
JDK's sentinel for its `NumberFormatException` arm. This one was not only a
wrong type: the digits were parsed into a `usize`, so on a 64-bit host
`String.format("%2147483648d", 1)` was an allocation of two billion spaces
rather than a refusal.

**`%.d`** — a `.` with no digits after it — → `UnknownFormatConversionException:
Conversion = '.'`. `parsePrecision` returns -1 for it and `parse()` then returns
0, which the outer loop reports as an unknown conversion naming the character
after the `%`. CratonVM read it as a precision of zero and formatted.

**`%t1`** no longer treats `1` as a date/time field. The prefix is only a prefix
when `isConversion(c1)` holds; otherwise the scanner reads the `t` itself as the
conversion, and `Conversion.isValid('t')` is false, so HotSpot answers
`Conversion = 't'` and does not consume the `1`.

**The whole `%t`/`%T` arm ran no legality checks at all.** `checkDateTime` is
now applied in the JDK's order — precision, then `DateTime.isValid` on the
field, then the bad-flag set `# + ' ' 0 , (`, then `'-'` without a width. So
`%.2tY` and `%,tY` were accepted and now refuse, and an unknown field is
`UnknownFormatConversionException: Conversion = 'tX'` rather than an invented
`IllegalArgumentException: Unknown date/time conversion '%tX'`. `DateTime`'s
valid set is transcribed as `FMT_DATETIME_FIELDS` from the JDK's own `switch`,
deliberately separate from the set `format_temporal_field` implements: whether a
field is an unknown *conversion* is a question about the specifier and must be
answered the same way whether or not this VM implements the field. The two sets
coincide today at all 31 characters.

Three more in the same arm, all of them the negative half answering "here you
go":

* an **absent argument** appended nothing and the result came back silently
  short; it is now `MissingFormatArgumentException`, the same rule the general
  conversions already had.
* a **null argument** was an `IllegalArgumentException`; `printDateTime` returns
  `"null"` before it looks at the field, and now so does this — padded to the
  field width by the same rule a real date is (`fmt_pad_to_width`, extracted for
  exactly that reason).
* a **non-temporal argument** was `IllegalArgumentException: … cannot be
  formatted as a date`, a message with no counterpart in the JDK. It is now
  `IllegalFormatConversionException` naming the **field** character —
  `printDateTime`'s `else` arm is `failConversion(c, arg)` and `c` for a
  date/time specifier is the field, not the `t`. So `String.format("%tY", "x")`
  is `Y != java.lang.String`.

## Messages

Every message was taken from the JDK 25 source of the exception class itself,
not from the javadoc. Two are easy to get subtly wrong and are worth recording:

* `UnknownFormatFlagsException` is `"Flags = " + flags` — **no quotes** — where
  `IllegalFormatFlagsException` and `DuplicateFormatFlagsException` both quote.
  (Excluded above, but the asymmetry is real and the next person to add it
  should not assume symmetry.)
* `IllegalFormatCodePointException` is `String.format("Code point = %#x", c)`
  over an `int`, and `%x` renders a negative `int` as unsigned 32-bit — so
  `%c` of `-1` reports `Code point = 0xffffffff`, which is why the Rust side
  casts to `u32` before formatting rather than after.

The nine pre-existing messages were re-checked against the same sources and all
nine already matched, including
`FormatFlagsConversionMismatchException`'s `"Conversion = " + c + ", Flags = "
+ f` field order and `MissingFormatWidthException`'s bare specifier text.

## Both modes, and why that is safe

This applies to Compatible mode (`--real-jdk`) as well as `--jdk-only`, and
says so deliberately: Compatible mode is contractually byte-for-byte frozen
except where a change is a strict bug fix, and this is one — it is HotSpot
parity in both the class and the message.

The specific reason it cannot break a caller: **every class raised here extends
`java.lang.IllegalArgumentException`, which is what was being raised before.**
Any `catch (IllegalArgumentException)` or `catch (RuntimeException)` that
caught the old behaviour still catches the new. The change is strictly
additive in what it makes catchable — a `catch
(MissingFormatArgumentException)` that could never fire now can. There is no
caller that was working and stops.

The one behaviour that is not merely a re-typing is the set of new *refusals*:
`%0$s`, `%c` of an out-of-range code point, an overflowing width or precision,
`%.d`, and the `%t` legality checks now throw where they previously produced a
value. Each of those is a format string HotSpot rejects, so a caller relying on
the old answer was already divergent — but it is a behaviour change and not
only a type change, and is named here rather than filed under the heading.

## Tests

No existing test asserted any of these exception types: a sweep of `*.rs`,
`*.java` and `*.txt` for the nine class names found only
`native-builtins/src/lang_string.rs` itself, this record and its four
predecessors, and one unrelated H2 note. So there was nothing to tighten and
nothing was weakened. `lang_string.rs`'s own `mod tests` covers only
`format_double` / `format_float` and is untouched.

No `CRATONVM_*` flag was added.

## Residuals

**A lone surrogate is still `?`.** `Character.isValidCodePoint` admits
`0xD800..=0xDFFF` and `Character.toChars` hands one back as a single unpaired
`char`, so HotSpot's `String.format("%c", 0xD800)` produces a string containing
that surrogate. Rust's `char` cannot hold one and `create_string` takes a
`&str`, so this range still yields `?`. It is now the *only* thing that yields
`?` — everything outside `0..=0x10FFFF` refuses. Fixing it needs a UTF-16-aware
string constructor and belongs with the string representation, not here.

**Unverified.** The record's status line is the residual that matters most: this
has not been built and the probe has not been re-run. The claim to check first
is the root cause — if `format.unknownConversion` still reports
`java.lang.IllegalArgumentException` after this, the fallback is still being
reached and the screens in `fmt_exception_class_available`, not the load, are
what to look at.

**The `%t` arm's other half is untouched.** `extract_temporal_fields` still
models every `Date`/`Calendar`/`Long` as a zero-offset wall clock, so `%tz` is
always `+0000` and `%tZ` is always `UTC`. That is a pre-existing timezone gap
with its own shape and is not a refusal question.

> **2026-08-12 — that gap now has a measured sibling, and the `%t` arm's THIRD
> half turned out to be worse than either.** (1) The timezone gap is not
> confined to `extract_temporal_fields`' own model:
> `ZoneId.systemDefault()` answers `UTC` on every Windows host, so even a
> `ZonedDateTime` — which this arm reads correctly, field by field — arrives
> already three hours wrong on this UTC+3 machine. Producer and measurement in
> `W7-91-format-date-symbols-hardcoded-english.md` §4. (2) The *name* fields
> this record's legality checks were placed around (`%tB` `%tb`/`%th` `%tA`
> `%ta` `%tp` `%tr` `%tc`) rendered from four hard-coded English arrays in
> every locale; they now go through `java.text.DateFormatSymbols`, which is what
> `Formatter.print(TemporalAccessor, char, Locale)` does. Same record. Nothing
> in this record's refusal set moved: the new code is inside the arms
> `FMT_DATETIME_FIELDS` has already admitted, so which refusal a bad specifier
> gets is unchanged. One rendering did change beyond the locale —
> `%tc`'s day is now zero-padded, which is `DAY_OF_MONTH_0`, the composite's
> own rule.

**Verified against JDK 25's source 2026-08-12** (`jdk25src`, this host), because
the whole record turns on getting the class right per case: `IllegalFormatException`
is `sealed ... permits` exactly the twelve named in the table above, in that
spelling; `UnknownFormatFlagsException.getMessage()` really is the unquoted
`"Flags = " + flags` where `IllegalFormatFlagsException` and
`DuplicateFormatFlagsException` both quote; and
`IllegalFormatArgumentIndexException` really is a package-private `final class`
with a package-private constructor. The source side is present too —
`fmt_exception_class_available`, the `FmtFault` enum and both previously
unraised classes are in `native-builtins/src/lang_string.rs`. What is still
unverified is only the run.

**The `%t` argument-type check is by class name, not by interface.**
`extract_temporal_fields` accepts `Long`, `java.util.Date`, `Calendar` and a
hand-listed set of `java.time` classes. HotSpot accepts any `TemporalAccessor`.
A `TemporalAccessor` outside that list now gets
`IllegalFormatConversionException` where HotSpot would format it — a better
*type* than before, but still a refusal HotSpot does not make. Widening the
accept set is a separate change and wants its own measurement.
