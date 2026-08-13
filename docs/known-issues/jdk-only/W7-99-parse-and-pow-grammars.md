# W7-99 — `Math.pow`, `Math.ulp`, and the numeric parse grammars

**Status:** fixed in `native-builtins/src/lang_math.rs`. **MEASURED on a real
binary 2026-08-12** — see the reconciliation banner.

> ## RECONCILED 2026-08-12 (lane C18)
>
> The "awaiting a rebuild to re-measure" hedge this record shipped with is
> **discharged for all three families**, and one of the three has to be split:
>
> * **`Math.pow` special values — FIXED and VERIFIED on a real binary.
>   MEASURED.** §1 is closed.
> * **`Math.pow` is nevertheless not one item.** A *second*, independent
>   `Math.pow` defect was found afterwards and is **NOT** covered by §1: the
>   fast path took `a.powi(b as i32)` (binary exponentiation) for integral
>   exponents, measured at **1.4 ulp at |b| = 2 and 44.3 ulp at |b| = 63
>   against `Math.pow`'s 1-ulp contract**. It is on **ordinary** inputs, which
>   is exactly why a special-value census missed it. See
>   `W7-95-intrinsic-semantics-census.md` §"`Math.pow`: the special values
>   were the smaller half". Do not read §1's closure as closing "pow".
> * **`Math.ulp` is NOT a defect. MEASURED**, by exhaustive proof over all
>   4,294,967,296 `float` bit patterns, not by sample. §2 below is the
>   pre-fix state.
> * **`Math.floorDiv(MIN_VALUE, -1)`** (§"Unchecked-arithmetic sweep")
>   — **FIXED and VERIFIED on a real binary. No family aborts the VM any
>   more.**
**Found by:** the 755-row raw-bit `Intrinsic` differential census against
HotSpot 25 (2026-08-12). 677 rows agreed; these are three of the 39 divergent
triples.
**Lane:** B14.

## The shape

Three unrelated-looking defects, one root cause: **a Rust standard-library
routine was used where a Java specification was meant.** In each case the Rust
routine is correct for its own standard and wrong for Java's, and in each case
the divergence is a wrong *value* rather than a missing exception — so nothing
short of a raw-bit differential finds it.

Pure IEEE-754 arithmetic in the interior of its domain was already right: 259
`Math`/`StrictMath` rows over NaN/±0.0/±Infinity/overflow boundaries agreed
exactly. The failures cluster on the seams where Java *defines* something that
C99 or Rust defines differently.

## 1. `Math.pow` used C99's special-value table, not the JLS's

`Math.pow` fell through to Rust's `f64::powf`, which lowers to C99 `pow()`.
C99 and the JLS disagree on two rules:

| | C99 / `powf` | JLS |
|---|---|---|
| `pow(1.0, NaN)` | `1.0` | NaN |
| `pow(±1.0, ±Infinity)` | `1.0` | NaN |

C99 makes `pow(1, y)` unconditionally 1.0 on the reasoning that 1 raised to
anything is 1. The JLS instead says "if the second argument is NaN, the result
is NaN" with no exception for a base of 1, and separately "if the absolute
value of the first argument equals 1 and the second argument is infinite, the
result is NaN".

The body's own comment **named the exact rule it was breaking** — it claimed
the fall-through to `powf` was "preserving Java/JLS special-value semantics
(e.g. `pow(1, ±inf) == NaN` per JLS)". `powf` is where that rule is lost, not
where it is preserved.

`StrictMath.pow` was already correct: it routes through the ported fdlibm in
`types/src/fdlibm.rs`. Two bodies for one operation in one file, only one of
them audited.

**Fix:** pre-empt `powf` on exactly the two disagreeing predicates. The
neighbouring rule `pow(a, ±0.0) == 1.0` — which holds *even for a NaN base* —
is shared by both standards and is deliberately left to fall through.

| row | HotSpot 25 | before | after |
|---|---|---|---|
| `pow(1.0, NaN)` | `0x7ff8000000000000` | `0x3ff0000000000000` | `0x7ff8000000000000` |
| `pow(1.0, +Inf)` | `0x7ff8000000000000` | `0x3ff0000000000000` | `0x7ff8000000000000` |
| `pow(1.0, -Inf)` | `0x7ff8000000000000` | `0x3ff0000000000000` | `0x7ff8000000000000` |
| `pow(-1.0, +Inf)` | `0x7ff8000000000000` | `0x3ff0000000000000` | `0x7ff8000000000000` |
| `pow(-1.0, -Inf)` | `0x7ff8000000000000` | `0x3ff0000000000000` | `0x7ff8000000000000` |

22 further `pow` rows (including `pow(NaN, 0.0) == 1.0`, `pow(-2, 0.5)`,
`pow(-0.0, -1)`, `pow(10, 100)`) already agreed and must keep agreeing.

### Do not "fix" `Math.cosh`

`Math.cosh(1.0)` is 1 ULP from HotSpot, which is inside `Math`'s documented
2.5-ULP allowance, and `StrictMath.cosh` matches exactly. That row is not a
defect.

### NaN payloads are not a contract

HotSpot's own `Math.pow` returns `0x7ff8000000000000` for `pow(1.0, +Inf)` but
`0xfff8000000000000` (negative NaN) for `pow(-2.0, 0.5)`, and its
`StrictMath.pow(1.0, +Inf)` returns `0xfff8000000000000`. We match all three
today by accident of which routine produces them. Only "is NaN" is specified;
do not build a gate on the payload bits.

## 2. `Math.ulp`/`StrictMath.ulp` returned `+Infinity` at `MAX_VALUE`

`ulp(x)` was computed as `nextUp(|x|) - |x|` via `from_bits(bits + 1)`. At
`MAX_VALUE` the bit pattern `bits + 1` **is** the encoding of `+Infinity`, so
the subtraction became `Infinity - MAX_VALUE == Infinity`.

The step at the top binade has to run backwards. `MAX_VALUE - nextDown(MAX_VALUE)`
is exact and representable.

| row | HotSpot 25 | before | after |
|---|---|---|---|
| `Math.ulp(Double.MAX_VALUE)` | `0x7ca0000000000000` | `0x7ff0000000000000` | `0x7ca0000000000000` |
| `Math.ulp(-Double.MAX_VALUE)` | `0x7ca0000000000000` | `0x7ff0000000000000` | `0x7ca0000000000000` |
| `StrictMath.ulp(Double.MAX_VALUE)` | `0x7ca0000000000000` | `0x7ff0000000000000` | `0x7ca0000000000000` |
| `Math.ulp(Float.MAX_VALUE)` | `0x73800000` | `0x7f800000` | `0x73800000` |
| `Math.ulp(-Float.MAX_VALUE)` | `0x73800000` | `0x7f800000` | `0x73800000` |
| `StrictMath.ulp(Float.MAX_VALUE)` | `0x73800000` | `0x7f800000` | `0x73800000` |

`0x7ca0000000000000` is 2^971; `0x73800000` is 2^104. The other 14 `ulp` rows
(zero, subnormal, `MIN_NORMAL`, 1.0, NaN, ±Infinity) already agreed.

## 3. The parse grammars were Rust's, not Java's

### 3a. `Integer.parseInt` — wrong in *both* directions

The body was `text.trim().parse::<i32>()`.

* **The `.trim()` invented an acceptance Java does not have.** Java's integer
  grammar is `Signopt Digit+` with no whitespace anywhere.
  `Integer.parseInt("  1")`, `("1 ")`, `("1\n")`, `("\t1")` all throw
  `NumberFormatException` on a real JDK; we answered `1`. The `.trim()` is
  `Double.parseDouble`'s contract — whose grammar really does skip
  `[\x00-\x20]*` on both ends — copied onto the integer one, where it does not
  belong. The radix overload had the same bug (`parseInt("  ff", 16)`).
* **`parse::<i32>` rejects the Unicode decimal digits Java accepts.**
  `Integer.parseInt("١٢") == 12` on a real JDK (ARABIC-INDIC ONE
  TWO); likewise Devanagari, fullwidth, and 35 other blocks.

Both directions matter. Code that calls `parseInt` inside a `try` specifically
to *reject* junk was getting junk accepted.

| row | HotSpot 25 | before | after |
|---|---|---|---|
| `parseInt("  1")` | NFE | `1` | NFE |
| `parseInt("1 ")` | NFE | `1` | NFE |
| `parseInt("1\n")` | NFE | `1` | NFE |
| `parseInt("\t1")` | NFE | `1` | NFE |
| `parseInt("١٢")` | `12` | NFE | `12` |
| `parseInt("५")` | `5` | NFE | `5` |
| `parseInt("７")` | `7` | NFE | `7` |
| `parseInt("  ff", 16)` | NFE | `255` | NFE |
| `parseInt("١٢", 10)` | `12` | NFE | `12` |

**The digit table is BMP-only on purpose.** `Integer.parseInt` walks the string
with `charAt` and calls the `char` overload of `Character.digit`, so a
*supplementary* decimal digit arrives as a surrogate pair and matches nothing.
Measured on JDK 25: `Integer.parseInt(new String(Character.toChars(0x104A0)))`
throws even though `Character.digit(0x104A0, 10) == 0`. Adding the
supplementary runs would make us **more** permissive than Java.

The table was generated from JDK 25 itself rather than hand-copied from
Unicode data, by walking every code point and recording each maximal run over
which `Character.digit(cp, 36)` increases by one — 80 runs, of which the 38
non-ASCII BMP ones are what the table carries.

**The same broken shape was on all eight signed parsers**, not just the one
the census caught: `Integer.parseInt`, `Long.parseLong`, `Byte.parseByte`,
`Short.parseShort` and their radix overloads. All eight now share one grammar.

Two JDK details that only show up once the family is unified:

* `Byte`/`Short` are **two-step** in the JDK — they call `Integer.parseInt`
  and *then* range-check — and that ordering is observable in the detail
  message. `Byte.parseByte("999")` reports `Value out of range. Value:"999"
  Radix:10`, but `Byte.parseByte("99999999999999999999")` reports `For input
  string: …` because it fails at int width first.
* `NumberFormatException.forInputString` appends ` under radix N` for every
  radix except 10.

### 3b. `Double.parseDouble` / `Float.parseFloat` — also both directions

`parse_double_string` had explicit `"NaN"`/`"Infinity"` arms and then fell
through to `parse::<f64>()`.

* Rust accepts `nan`, `inf`, `infinity` **case-insensitively**; Java accepts
  only the exact spellings `NaN` and `Infinity`. We were returning
  `+Infinity` from `Double.parseDouble("inf")`, which a real JDK rejects.
* Rust **rejects** Java's hex significand: `Double.parseDouble("0x1p3") == 8.0`.
* Rust's `str::trim` strips Unicode whitespace; the grammar's `[\x00-\x20]*`
  does not. `Double.parseDouble(" 1.0")` throws on a real JDK.
* `Double.parseDouble(null)` / `Float.parseFloat(null)` throw
  **`NullPointerException`**, not `NumberFormatException` — they reach
  `String.length()` before any grammar check. (The *integer* family is the
  other way round: `Integer.parseInt(null)` throws
  `NumberFormatException("Cannot parse null string")`. Both measured.)

The grammar implemented is the regex published in the `Double.valueOf(String)`
javadoc, transcribed rather than approximated. One consequence worth pinning:
`Digits` there is `\p{Digit}`, which **without** `UNICODE_CHARACTER_CLASS` is
ASCII `[0-9]` only — so unlike `parseInt`, the floating-point grammar does
*not* take Unicode digits. `Double.parseDouble("١٢")` throws while
`Integer.parseInt("١٢")` returns 12. The two grammars genuinely
disagree; copying one onto the other is how this drifted in the first place.

| row | HotSpot 25 | before | after |
|---|---|---|---|
| `pd("nan")` | NFE | `0x7ff8000000000000` | NFE |
| `pd("NAN")` | NFE | `0x7ff8000000000000` | NFE |
| `pd("inf")` | NFE | `0x7ff0000000000000` | NFE |
| `pd("Inf")` | NFE | `0x7ff0000000000000` | NFE |
| `pd("infinity")` | NFE | `0x7ff0000000000000` | NFE |
| `pd("+inf")` | NFE | `0x7ff0000000000000` | NFE |
| `pd("-inf")` | NFE | `0xfff0000000000000` | NFE |
| `pd(" 1.0")` | NFE | `0x3ff0000000000000` | NFE |
| `pd("0x1p3")` | `0x4020000000000000` | NFE | `0x4020000000000000` |
| `pd("0x1.8p1")` | `0x4008000000000000` | NFE | `0x4008000000000000` |
| `pd("-0x1p-1")` | `0xbfe0000000000000` | NFE | `0xbfe0000000000000` |
| `pd("0X1P3")` | `0x4020000000000000` | NFE | `0x4020000000000000` |
| `pd(null)` | NPE | NFE | NPE |
| `pf("nan")` | NFE | `0x7fc00000` | NFE |
| `pf("inf")` | NFE | `0x7f800000` | NFE |
| `pf("infinity")` | NFE | `0x7f800000` | NFE |
| `pf("0x1p3")` | `0x41000000` | NFE | `0x41000000` |
| `pf(null)` | NPE | NFE | NPE |

Rows that already agreed and must keep agreeing: `"  1.5  "` (the float
grammar *does* trim), `"1d"`, `"1f"`, `"1."`, `".5"`, `"1e3"`, `"-0.0"`,
`"1e400"` → `+Infinity`, `"1e-400"` → `0.0`, `"1_0"` → NFE, `"1e"` → NFE,
`"0x10"` → NFE (no `p`), `"١٢"` → NFE.

#### Why the hex path is hand-rolled

Rust has no hex-float parser, so `M * 2^(p - 4·fracDigits)` is rounded to the
target width directly: the significand accumulates into a `u128` with a sticky
bit for anything past 124 bits, then one round-to-nearest-ties-to-even step
lands on the result. Two subtleties that a naive version gets wrong:

* **The quantum changes at the subnormal boundary.** In the normal range it
  tracks the exponent; below `MIN_NORMAL` it is pinned at 2^-1074 (2^-149 for
  `float`). Renormalizing a carry the normal way in the subnormal range is
  wrong — and a carry that reaches exactly 2^52 needs *no* renormalization,
  because the subnormal→normal transition is seamless in the IEEE-754 bit
  encoding. The first draft of this code got `0x1.8p-1074` and `0x3p-1075`
  wrong for exactly this reason; the fuzz below caught both.
* **`float` is parsed at float width, never by narrowing a `double`.** Two
  roundings can land on a different float than one.

#### How it was verified without a build

This lane could not run `cargo`. The algorithm was therefore written first as
a line-for-line Java prototype (`BigInteger` capped at 128 bits standing in for
`u128`, so the arithmetic is bit-identical) and fuzzed against HotSpot's own
`Double.parseDouble` / `Float.parseFloat` / `Integer.parseInt` /
`Long.parseLong`:

**2,615,495 cases, 0 mismatches**, covering both widths; the subnormal band and
both overflow edges at every exponent; exact ties at every subnormal quantum;
a double-rounding probe with >24-bit significands at float width; and the
integer grammar across all radices 2..=36.

The Rust is a transcription of that validated prototype. It still needs a
build to confirm it compiles and to re-run the census.

## Duplicate-registration check

Several `java.lang.*` triples are registered twice — `Intrinsic` in
`lang_math.rs` and a `Bridge` copy in `lib.rs`, which wins under
last-write-wins. Four such duplicates exist for `Character` and would have
silently masked a fix. **Every triple touched here was checked:**

* `Math.pow`, `StrictMath.pow`, `Math.ulp`, `StrictMath.ulp`,
  `Double.parseDouble`, `Float.parseFloat` — **no second registration
  anywhere in the repo.**
* `Integer.parseInt(String)I` is registered a second time, at
  `native-builtins/src/lib.rs:7896` — but it re-registers **the same function
  pointer** (`crate::lang_math::native_integer_parse_int`), so it is a
  re-registration, not a copy, and the fix flows through it.
* There is also a *third* path,
  `native-builtins/src/intrinsics/mod.rs:86` →
  `intrinsics/integer.rs:72`, and a JIT trampoline at `intrinsics/mod.rs:289`.
  Both are pure delegates to the same `native_integer_parse_int`.

So no fix here is masked, and no deletion needs nominating.

## Unchecked-arithmetic sweep

A VM abort outranks a wrong answer, and the `Math.floorDiv(MIN_VALUE, -1)`
abort fixed earlier today came from this same family of assumptions. The whole
file was re-audited for panic paths reachable from ordinary bytecode —
signed `/` and `%`, `abs`/negation at `MIN_VALUE`, overflowing `+`/`*`/`<<`,
the radix-taking APIs that panic outside 2..=36, indexing, and `unwrap`.

**No unguarded site remains.** Notably `from_str_radix` — the routine behind
the `parseInt("5", 0)` abort — is no longer called anywhere in the file, since
the parse family now goes through the hand-written digit decoder, which
returns `None` for a bad radix instead of panicking.

The one new place where a Java-supplied string drives a shift *count* is the
hex-float rounder, and its shift invariant is now pinned in a comment at the
function: three interlocking early returns are what keep every shift below the
integer width, and loosening any of them reintroduces an abort reachable from
`Double.parseDouble`.

## Residuals

* **Not yet rebuilt.** Every "after" column above is the fuzz-validated
  prototype's answer, not a measurement from a CratonVM binary. The census
  needs re-running once `native-builtins` is built.
* `Integer.getInteger` / `Long.getLong` (`lang_math.rs` ~3072, ~3585) still do
  `raw.trim().parse::<…>()` on a system-property value. These were **not**
  changed: their contract is `Integer.decode`, a third grammar again (it takes
  `0x`, `#`, and leading-`0` octal), so they need their own fix rather than
  this one.
* `Integer.parseUnsignedInt` / `Long.parseUnsignedLong` are not registered in
  this file; if they are ever added they need the same grammar, plus the
  JDK's rule that `"-0"` is rejected.
* `Character.digit` itself (`native_character_digit`) is still backed by
  Rust's `char::to_digit` and so does **not** know the Unicode runs — it is
  owned by the sibling `Character` lane and was deliberately left alone. It is
  now inconsistent with `parseInt`, which does know them; the table added here
  (`JAVA_DIGIT_RUNS` + `java_char_digit`) is the helper that lane should reuse.

## Functions left to the `Character` lane

Untouched by this lane: `native_character_digit`, `native_character_for_digit`,
`native_character_get_numeric_value`, and every other `native_character_*`
body, plus `case_map.rs`.
