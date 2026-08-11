# W7-3 — `String.format`'s float conversions, and the `StringBuilder` bounds checks that were not there

**Status: source landed, UNVERIFIED.** Nothing in this record has been built or
run against a VM. It takes two of the four families in
`docs/known-issues/jdk-only/W7-1-treemap-views-and-iterator-remove-contract.md`
(family 3, the float conversions; family 4, the `StringBuilder` bounds), both
measured 2026-08-10 by `probes/ShadowDifferentialProbe.java` in `--real-jdk`
mode against HotSpot 25 on Linux. The TreeMap-view and `Iterator.remove`
families are somebody else's lane and are untouched here.

Everything below is in `native-builtins/src/lang_string.rs`.

## What W7-1 measured

| observable | HotSpot 25 | CratonVM before |
|---|---|---|
| `String.format("%.3f\|%e\|%g", 1.0/3, 1234.5, 0.0001)` | `0.333\|1.234500e+03\|0.000100000` | `0.333\|1.2345e3\|1.0E-4` |
| `new StringBuilder("ab").delete(5, 6)` | `StringIndexOutOfBoundsException` | no-throw |

---

# Family 3 — the float conversions

## Four defects, not one

The probe's one line contains three, and reading the code found a fourth.

1. **`%e` was Rust's `{:e}`.** Rust writes the exponent bare and unpadded
   (`e3`); `java.util.Formatter` writes it always-signed and at least two digits
   (`e+03`).
2. **`%e` had no default precision.** The precision block was gated on
   `precision.is_some()`, so a bare `%e` never entered it at all and fell
   through to `format_arg`'s `{:e}` — the shortest round-trip mantissa, where
   Formatter's default is 6 digits after the point. **A default that only exists
   on the with-precision branch is not a default**, and this is the shape to
   look for in the rest of the formatter: the `_ =>` arm of a conversion table
   is where a missing default hides, because it looks like a fallback rather
   than a decision.
3. **`%g` was two different wrong things.** With a precision it went to
   `{:.prec$}` — fixed notation, i.e. `%f`. Without one it fell through to
   `format_double`, i.e. `Double.toString` (hence `1.0E-4`). Formatter's `%g` is
   a third algorithm: it picks scientific or fixed by the exponent of the
   **rounded** magnitude against the precision, and — unlike C's `%g` — never
   strips trailing zeros. That is the whole of why `%g` of `1e-4` is
   `0.000100000` and not `0.0001`.
4. **`%a` had no arm.** It was accepted by the spec parser, then fell through
   the precision block's `_ =>` to `{:.prec$}` and through `format_arg`'s `_ =>`
   to `Double.toString`. Neither is a hex float.

## The fifth, which the probe could not see

Rust rounds ties to **even**. `java.util.Formatter` specifies **HALF_UP** for
`%e`/`%f`/`%g`. So `%.1f` of `0.25` was `0.2` where HotSpot says `0.3`, and
`%.0f` of `2.5` was `2` where HotSpot says `3` — on the conversion W7-1's table
lists as *matching*. `%f` matched on `1.0/3` because 1/3 has no tie anywhere
near the third decimal; a divergence that only appears at exact ties will pass
every probe whose inputs are not chosen to hit one.

`%f` was therefore rewritten too, even though W7-1 does not name it. It is the
same match arm and the same rounding rule.

## How it is implemented, and why that way

The rounding rule is the thing that decides the shape. Rust's formatter cannot
be asked for HALF_UP, so nothing below asks it to round at all.

Every finite double is `m · 2^k` for integers `m`, `k`. Once `m` is odd, `2^k`
with `k < 0` is `5^-k / 10^-k`, so the exact decimal expansion **terminates**
after exactly `-k` fractional digits — at most 1074, for the smallest subnormal.
`fmt_exact_fraction_digits` reads `k` out of the bits; `fmt_exact_decimal` then
asks Rust for precisely that many fractional digits, which is a request Rust
answers exactly (flt2dec's exact mode is not a shortest-representation
approximation). The result is the true decimal expansion with no rounding
applied, as a digit vector plus a base-10 exponent.

From there HALF_UP is one comparison — `fmt_round_significant` rounds up exactly
when the first discarded digit is >= 5, including the "5 followed by nothing"
tie that is the only case half-even decides differently — and every conversion
is assembly:

| conversion | rounding | rendering |
|---|---|---|
| `%f` | `fmt_round_at_fraction` (precision counts digits after the point) | `fmt_render_fixed` |
| `%e` | `fmt_round_significant` to `precision + 1` | `fmt_render_scientific` |
| `%g` | `fmt_round_significant` to `precision`, then the branch | `fmt_render_fixed` or `fmt_render_scientific` |
| `%a` | `fmt_hex_float` — see below | `fmt_hex_digits` |

`%g`'s branch is the JDK's: after rounding, `-4 <= exp < precision` prints fixed
with `precision - 1 - exp` fraction digits, anything else prints scientific with
`precision - 1`. Deciding on the **rounded** exponent is load-bearing — `%g` of
`999999.5` rounds to `1000000`, whose exponent 6 is no longer less than the
precision 6, so it prints `1.00000e+06` and not `1000000`.

Zero is special-cased in `%g` for the same reason the JDK special-cases it:
mantissa `0` with a rounded exponent of `0` lands in the fixed branch, so `%g`
of `0.0` is `0.00000` (five zeros, not six) and never `0.000000e+00`.

**`%a` is the one member of the family that is not HALF_UP.** The JDK rounds it
in *binary*, half to even, inside `Formatter.hexDouble`, and then re-renders the
rounded double through `Double.toHexString` — so nothing pads the digits back
out, and `%.4a` of `1.0` is `0x1.0p0`, not `0x1.0000p0`. `fmt_hex_float`
reproduces that, including the subnormal `2^54` normalise-and-subtract-54 dance
and the `1.0p1024` string the JDK hard-codes for a rounding carry that leaves
the exponent field. Sharing the decimal path's HALF_UP here would have been the
tidier-looking answer and the wrong one.

`Double.toString`'s own `10^-3 .. 10^7` scientific threshold (`format_double`,
`cratonvm_types::java_double_to_string`) is deliberately **not** shared with any
of this. It is a different set of rules: `%g` at the default precision switches
to scientific below `10^-4`, not below `10^-3`.

## Also changed in the flag layer

The flags and width run after the raw conversion and were mostly left alone.
Three exceptions:

* `%a`/`%A` were missing from the `+` sign set. Formatter puts the sign outside
  the `0x` prefix (`+0x1.0p0`), which is where prepending to the whole
  conversion lands it.
* `%g`/`%G` were missing from the `0` zero-pad set, which Formatter accepts for
  them.
* Zero padding now requires the conversion to end in a digit. Formatter only
  pads inside its *finite* branch; `Infinity` and `NaN` reach the width
  justifier and get spaces. Before this, `%08e` of `Double.POSITIVE_INFINITY`
  would have produced `Infinity` with zeros prepended.

A null argument with a precision (`String.format("%.2f", (Object) null)`) now
keeps `format_arg`'s `"null"`. Formatter's `printFloat` prints `"null"` before
it looks at the conversion at all; the old code ran `extract_float_value` on the
null, got `0.0`, and printed `0.00`.

## What is verified and what is not

Verified: the seven new pure functions were extracted into a standalone file,
compiled with `rustc --edition 2021`, and run over 37 cases. The whole edited
`lang_string.rs` was parse-checked with `rustfmt --emit stdout` on a **copy**
(the tree is not fmt-clean; nothing was reformatted in place).

Those 37 outputs agree with `java.util.Formatter`'s javadoc — including the
three W7-1 cells (`0.333`, `1.234500e+03`, `0.000100000`), the HALF_UP ties
(`%.0f` of `0.5`/`1.5`/`2.5` -> `1`/`2`/`3`, `%.1f` of `0.25` -> `0.3`), `-0.0`
keeping its sign, `%g` of `0.0` -> `0.00000`, `%g` of `999999.5` ->
`1.00000e+06`, `%e` of `Double.MIN_VALUE` -> `4.940656e-324`, `%a` of `1e-4` ->
`0x1.a36e2eb1c432dp-14`, and `%.3a` of `Double.MIN_VALUE` -> `0x1.0p-1074`.

**Not verified:** none of this has been compiled as part of the crate, and no
oracle run against HotSpot 25 has been made. The javadoc was the oracle, not a
measurement.

## One known residual, stated because it is a real difference

Beyond roughly **20 significant digits** this implementation is *more* exact
than HotSpot, which is still a divergence.

`FloatingDecimal.BinaryToASCIIBuffer` holds its digits in a `char[20]`, so
`Formatter` appears to work from at most 20 significant digits and zero-fill
past that. This implementation works from the full exact expansion. The two
agree for every precision <= ~17 and every magnitude below ~10^20 — which is all
ordinary formatting, and all of W7-1's cases — and can differ in the low-order
digits of, specifically:

* `String.format("%f", 1e300)` — the 281 digits after the first 20 of the
  integer part;
* `String.format("%.30f", 0.1)` — the digits past the 20th.

The 20-digit cap was **not** confirmed against JDK 25, which is exactly why it
was not emulated: guessing the cap wrong would put a divergence into the common
case to remove one from the rare case. Whoever next has a HotSpot 25 to hand
should run those two expressions; if the cap is real, the fix is a truncate-and-
zero-fill step inside `fmt_exact_decimal`, and nothing else changes.

---

# Family 4 — `StringBuilder` / `StringBuffer` bounds

## The species

This is `docs/known-issues/jdk-only/W2-7-fabricated-success-where-the-spec-mandates-failure.md`
one more time: the negative half of the API, the part that asserts a failure
fails correctly, answered "here you go".

`delete` is the clean statement of it. `AbstractStringBuilder.delete` clamps
`end` **down** to the length and only then runs
`checkRangeSIOOBE(start, end, count)`. So an over-long `end` alone is legal
(`delete(0, 100)` empties the builder) while a `start` past the length is not:
after the clamp, `delete(5, 6)` on `"ab"` is `start 5 > end 2`. CratonVM clamped
**both** ends, which turns every out-of-range delete into a silent no-op. A
caller that walks a builder and guards itself with the exception never leaves
the loop.

`setCharAt` was the worst of them: an out-of-range index dropped the store and
returned normally. That is a **write that silently did not happen**.

## What landed

Every row is `native-builtins/src/lang_string.rs`. "Was" describes the code
before this change.

| method | was | now | exception |
|---|---|---|---|
| `delete(int,int)` | clamped both ends | clamp `end`, then `checkRangeSIOOBE` | `StringIndexOutOfBoundsException` |
| `deleteCharAt(int)` | returned `this` untouched | `checkIndex(index, count)` | `StringIndexOutOfBoundsException` |
| `replace(int,int,String)` | clamped both ends; null replacement became `""` | clamp `end`, then `checkRangeSIOOBE`; then null -> NPE | `StringIndexOutOfBoundsException`, then `NullPointerException` |
| `insert(int,String)` | clamped offset | `checkOffset(offset, count)` | `StringIndexOutOfBoundsException` |
| `insert(int,char)` | clamped offset | `checkOffset` | `StringIndexOutOfBoundsException` |
| `insert(int,int)` | clamped offset | `checkOffset` | `StringIndexOutOfBoundsException` |
| `insert(int,Object)` | clamped offset | `checkOffset` | `StringIndexOutOfBoundsException` |
| `insert(int,char[])` | clamped offset | `checkOffset` | `StringIndexOutOfBoundsException` |
| `insert(int,char[],int,int)` | clamped offset AND the source window | `checkOffset`, then the source range | `StringIndexOutOfBoundsException` |
| `setCharAt(int,char)` | dropped the store | `checkIndex(index, count)` | `StringIndexOutOfBoundsException` |
| `setLength(int)` | clamped a negative to 0 | reject negative | `StringIndexOutOfBoundsException` |
| `codePointCount(int,int)` | clamped `endIndex` | `checkRangeSIOOBE`, no clamp | `StringIndexOutOfBoundsException` |
| `append(char[],int,int)` | clamped the source window | `checkRange(offset, offset+len, str.length)` | `IndexOutOfBoundsException` |

`charAt`, `getChars`, `substring(int)`, `substring(int,int)`, `codePointAt` and
`codePointBefore` already had their checks and were not touched.

## Four things worth stating about the shapes

**The upper bound is not the same for `insert` and `deleteCharAt`.** `insert`'s
offset may equal the length (inserting at `count` appends); `deleteCharAt`'s
index may not. That is `checkOffset` versus `checkIndex`, and it is why
`sb_check_offset` exists rather than a reuse of `sioobe_index`. The JDK spells
`checkOffset(offset, length)` as
`Preconditions.checkFromToIndex(offset, length, length, SIOOBE_FORMATTER)`, so
its message names a *range*, not an index — the two checks do not merely differ
in strictness, they produce different text.

**Order is observable.** `insert(int,char[],int,int)` checks the destination
offset before the source window, so `insert(99, str, -1, 2)` on a short builder
reports the destination. `replace` runs its range check before it dereferences
the replacement, so `replace(5, 6, null)` on `"ab"` is an SIOOBE and not an NPE.
Both orders are the JDK's and both are reproduced.

**`insert(int,String)` and `replace(int,int,String)` disagree about null on
purpose.** `insert` substitutes `"null"`; `replace` reads `str.length()` with no
guard and throws. Making them consistent would be wrong.

**`offset + len` is computed in `i64`** in the two windowed methods. A wrapped
`int` sum reads as in-range, which is the same reason the JDK's own
`checkFromIndexSize` message prints the addition unevaluated.

## Deliberately left, with reasons

* **`append(CharSequence, int, int)` still clamps.** The comment on
  `native_sb_append_charsequence_off_len` records a prior session's deliberate
  choice — silent clamping keeps JUnit's error-reporting path alive, and every
  caller inside JDK internals passes in-bounds indices — and there is a unit
  test, `sb_append_charsequence_off_len_clamps_out_of_range`, pinning it. It is
  the same species and it is listed here so the inventory is honest, but
  reversing a tested, argued decision is not this lane's call.
* **`appendCodePoint(int)` still truncates an invalid code point** instead of
  throwing `IllegalArgumentException`. Same situation: an explicit comment
  argues for it on behalf of WHATWG callers.
* **`insert(int, boolean/long/float/double)` have no native override at all**,
  so they run real JDK bytecode against the synthetic `char[]`/`int` layout.
  That is a layout bug, not a bounds bug, and it is not in this lane.
* **`%a` with the `0` flag and a width** is still wrong: Formatter inserts the
  zeros *after* the `0x` prefix, and the generic width path cannot. `%a`/`%A`
  were left out of the zero-pad set rather than padded in the wrong place.

## Message wording: one unverified spot

`setLength(-1)` uses `StringIndexOutOfBoundsException(int)`'s classic wording
(`String index out of range: -1`) because that check is hand-rolled in the JDK
rather than routed through a `Preconditions` formatter. **Only the exception
class is pinned by the javadoc**; the text is unverified against JDK 25 and is
flagged as such in the code comment. Every other check in the table above goes
through `sioobe_index` / `sioobe_range`, whose text is already reproduced from
`Preconditions.outOfBoundsMessage` in `types/src/error.rs`.

---

## Out-of-file patch (not applied)

`docs/known-issues/jdk-only/W2-7-fabricated-success-where-the-spec-mandates-failure.md`
is not this lane's file. Its inventory table should gain these rows — thirteen
instances collapsed into three, since they are three checks and not thirteen:

```
| 6 | `StringBuilder`/`StringBuffer` `delete`/`deleteCharAt`/`replace`/`setCharAt`/`setLength` accepted any index and answered normally (`new StringBuilder("ab").delete(5, 6)` was a no-op; `setCharAt` past the end dropped the store) | `StringIndexOutOfBoundsException` | `native-builtins/src/lang_string.rs` | FIXED (W7-3, unverified) |
| 7 | All six `StringBuilder.insert` overloads clamped the destination offset, so an out-of-range insert appended at the end | `StringIndexOutOfBoundsException` (`checkOffset`) | `native-builtins/src/lang_string.rs` | FIXED (W7-3, unverified) |
| 8 | `append(char[],int,int)` / `insert(int,char[],int,int)` / `codePointCount` clamped their windows, so an out-of-range window silently used a SHORT slice | `IndexOutOfBoundsException` / `StringIndexOutOfBoundsException` | `native-builtins/src/lang_string.rs` | FIXED (W7-3, unverified) |
```

and its "how to find the next one" section is worth a fourth filter, which is
what actually found every row above:

> **A `min`/`clamp` on an argument that the spec range-checks.** `std::cmp::min(offset, chars.len())` is the same defect as a swallowed `Err`: it converts an argument the caller got wrong into an argument the callee is willing to accept. Grep for `.min(` and `std::cmp::min` in any native that shadows a JDK method with a documented `IndexOutOfBoundsException`, and ask what the JDK does — the answer is a throw far more often than a clamp, and the clamp is invisible at the call site in a way a wrong return value is not.

## Reproducing

Both families are in `probes/ShadowDifferentialProbe.java` already, in its
`stringSurface` section — `String.formatFloats` (line 530) and
`StringBuilder.deleteBad` (line 549):

```sh
javac -d /tmp/probes probes/ShadowDifferentialProbe.java
java -cp /tmp/probes ShadowDifferentialProbe > /tmp/hotspot.txt
cratonvm --real-jdk --java-home "$JAVA_HOME" -cp /tmp/probes ShadowDifferentialProbe \
  | grep -v '^\[cratonvm\]' > /tmp/cratonvm.txt
diff /tmp/hotspot.txt /tmp/cratonvm.txt
```

The probe does not yet cover the HALF_UP ties, `%a`, `%g`'s zero and
branch-boundary cases, or twelve of the thirteen bounds methods. Widening it to
those is the cheapest next step, and until that runs, everything in this record
is javadoc-derived rather than measured.
