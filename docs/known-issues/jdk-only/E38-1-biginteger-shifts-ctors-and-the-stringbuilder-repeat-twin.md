# E38-1 — the shift that allocates, the constructor that validated nothing, and the `repeat` twin that won

**Status: LANDED in `native-builtins/src/math_bignum.rs` and
`native-builtins/src/lang_string.rs` (the two files this lane owns), plus four
NOMINATIONS in files it does not. This lane cannot build or run CratonVM: every
statement about VM behaviour is labelled SOURCE-READ or PREDICTED. Every
statement about HotSpot is MEASURED on Microsoft OpenJDK 25.0.3+9, with the
transcript quoted.**

Follow-up to `W8-E29-1-bridge-census-round-1.md`, which source-read seven
`Bridge` families and found five with Java-reachable defects. This record takes
two of them — `BigInteger` and `AbstractStringBuilder` — from SOURCE-READ to
fixed, and corrects the census on one point of fact.

Probes: `scratchpad/e38/{Shift2,Ctor,Parity,ShiftParity,SbParity}.java`.

---

## 0. The correction: `unsigned_abs` is not the defect, and it is not in this file

The census says:

> `shiftLeft/shiftRight(Integer.MIN_VALUE)` uses `n.unsigned_abs()` → a
> `vec![0u32; 67_108_864]` (~256 MB) allocation instead of the specified
> `ArithmeticException`.

Three things in that sentence are wrong, and finding out which took the JDK
source plus one measurement each.

**(a) `unsigned_abs` is exactly right.** JDK 25 `BigInteger.java:3494-3506`:

```java
    } else {
        // Possible int overflow in (-n) is not a trouble,
        // because shiftRightImpl considers its argument unsigned
        return shiftRightImpl(-n);
    }
```

A negative distance flips the direction and `-n` is then read as an **unsigned**
32-bit count. `i32::unsigned_abs` is that widening
(`Integer.MIN_VALUE` → 2_147_483_648) and nothing else.

**(b) The allocating arm is `shiftRight`, not `shiftLeft`.** `shiftLeft(MIN)`
becomes a right shift, and a right shift by 2^31 bits clears every magnitude
word without allocating anything. `shiftRight(MIN)` becomes a **left** shift by
2^31 bits, and that is the 256 MB.

**(c) HotSpot does not answer 0 there — it throws, after allocating the same
256 MB itself.** MEASURED (`Shift2.java`, `-Xmx2g`):

```
ONE.shiftLeft(MIN) = 0   [0 ms]
ZERO.shiftLeft(MIN) = 0   [0 ms]
(-1).shiftLeft(MIN) = -1   [1 ms]
(-9).shiftLeft(MIN) = -1   [0 ms]
ZERO.shiftRight(MIN) = 0   [0 ms]
ONE.shiftRight(MIN) !! java.lang.ArithmeticException: BigInteger would overflow supported range   [157 ms]
(-1).shiftRight(MIN) !! java.lang.ArithmeticException: BigInteger would overflow supported range   [62 ms]
ONE.shiftLeft(MAX) !! java.lang.ArithmeticException: BigInteger would overflow supported range   [141 ms]
ONE.shiftRight(MAX) = 0   [0 ms]
(-1).shiftRight(MAX) = -1   [0 ms]
(-9).shiftRight(1) = -5   [0 ms]
16.shiftLeft(-2) = 4   [0 ms]
16.shiftRight(-2) = 64   [0 ms]
ONE.shiftLeft(1<<30) = <signum=1 bitLength=1073741825>   [74 ms]
```

The 157 ms and 62 ms rows are HotSpot allocating `new int[1 + 67_108_864]` and
then failing `checkRange` (`BigInteger.java:1213`):

```java
    if (mag.length > MAX_MAG_LENGTH || mag.length == MAX_MAG_LENGTH && mag[0] < 0)
        reportOverflow();
```

with `MAX_MAG_LENGTH == Integer.MAX_VALUE / 32 + 1 == 1 << 26`. The two arms
together say exactly **"magnitude bit length > `Integer.MAX_VALUE`"**, which is a
number you can compute from the operand's bit length and the shift distance
*without allocating*. The fix therefore refuses where HotSpot allocates-then-
refuses: same observable answer, no 256 MB.

**Why the census could not have seen this.** It read one registrar. There are
two.

---

## 1. `java/math/BigInteger` is registered TWICE, and the worse one won

| registrar | reached from | mode | representation |
|---|---|---|---|
| `phases_late::register_p71_biginteger_extras` | `register_essential_natives`, `lib.rs:7913` | **every** mode | `crate::bigint::BigInt` limbs, full two's complement |
| `math_bignum::register_biginteger_natives` | `register_synthetic_overrides`, `lib.rs:24010`, `#[cfg(feature = "synthetic-jdk")]` | synthetic-jdk only | decimal **strings** |

`register()` is last-registration-wins and 24010 > 7913, so **in synthetic-jdk
mode the decimal bodies overwrote the limb ones on fourteen triples.** This is
`[flag≠mode drops it]` and `[1 of 10 callsites]` in the same class: eight of the
fourteen decimal bodies open with

```rust
    let a = bi_read(ctx, this).trim_start_matches('-').to_string();
```

— they throw the SIGN AWAY and then compute. SOURCE-READ consequences, each
against a `RJdkBridge1 bigint` row:

| call | decimal body answers | HotSpot |
|---|---|---|
| `(-1).and(5)` | 1 | **5** |
| `(-2).or(1)` | 3 | **-1** |
| `(-9).bitCount()` | 2 | **1** |
| `(-1).bitCount()` | 1 | **0** |
| `(-1).bitLength()` | 1 | **0** |
| `ONE.testBit(-1)` | `false` (the `0..bit` loop is empty) | **ArithmeticException** |
| `(-1).toByteArray()` | `{0xFF, 0xFF}` | **`{0xFF}`** |
| `(0).not()` … `(-1).not()` | -1 … **-1** | -1 … **0** |
| `(-9).shiftRight(1)` | -4 (truncating) | **-5** (floor) |
| `ONE.shiftLeft(Integer.MIN_VALUE)` | **1** | **0** |

The last row is also a Rust hazard: the decimal `shiftLeft` computed `-n` on an
`i32`. `-i32::MIN` **overflows** — a panic in a debug build (and a panic is not a
Java throwable: it takes the VM down, it cannot be caught) and a silently empty
`0..i32::MIN` range in release. And `x.shiftLeft(Integer.MAX_VALUE)` was
`for _ in 0..n { result = bi_mul_unsigned(&result, "2") }`: 2^31 arbitrary-
precision decimal multiplies over a string that doubles in value each time. That
is a worse denial of service than the 256 MB one, reachable from one ordinary
call, and it was the *winning* implementation in synthetic-jdk mode.

### What landed

**Eight registrations deleted** from `register_biginteger_natives` — `and`,
`or`, `xor`, `not`, `bitLength`, `bitCount`, `testBit`, `toByteArray`. Nothing
replaces them: the limb twin registered earlier by
`register_p71_biginteger_extras` now wins in **both** modes, so there is one
implementation per triple instead of two that disagree. Consolidation, not
repair — the only move that has actually reduced this defect class.

That deletion is also `[1 of 10 callsites]` closing on itself: the correct
`toByteArray` converter, `bi_to_byte_array_str`, **lives in `math_bignum.rs`**,
is what `phases_late`'s registration calls, and was being shadowed by an inline
body in the same file.

**`shiftLeft`/`shiftRight` kept and rewritten**, on limbs, via two new helpers:

* `bi_shift_arg(n) -> (is_left, u32)` — the JDK's unsigned-distance rule, with
  the JDK's own comment quoted at it;
* `bi_checked_shl(v, k)` — `ArithmeticException("BigInteger would overflow
  supported range")` when `magnitude_bits(v) + k > Integer.MAX_VALUE`, decided
  from the bit count, before any allocation.

They are kept rather than deleted **because deleting them would hand
synthetic-jdk mode the 256 MB allocation that still lives in `phases_late`**
(NOMINATION 1). When that lands, these two should be deleted too and the file
will have zero overlap with `p71`.

### Evidence that the new shift is right

`ShiftParity.java` transliterates `bi_shift_arg` + `bi_checked_shl` +
`BigInt::{shl,shr}` into Java statement-for-statement and diffs the result
against `java.math.BigInteger` as `(signum, bitLength, low 64 bits)`:

```
SHIFT PARITY cases=1044 diffs=0 skipped(too big to materialise)=0
```

18 operands (zero, ±1, ±2, ±9, ±16, 255, `Long.MIN_VALUE`, `Long.MAX_VALUE`,
±2^100, ±(2^100-1), ±a 30-digit value) × 29 distances (`Integer.MIN_VALUE`,
`MIN_VALUE+1`, -2147483000, ±2^20, ±2^16, the word boundaries at ±31/32/33 and
±63/64/65, 0, …) × both methods. The rows whose results exceed 2^26 bits are
compared analytically rather than materialised, and they include every
near-overflow row.

**This proves the constants and the control flow. It does not prove the Rust
compiles, and it does not prove the Rust says what the Java says** — the
transliteration was done by hand. Note that the harness's *first* run reported
284 diffs, all of them the harness's own bug: it wrote `n & 0xFFFFFFFF` where
`unsigned_abs` is `(-n) & 0xFFFFFFFF`. `[a probe's setup is code that can be
wrong]` — twice, since a second round of 5 diffs was the harness comparing
`bitLength()` against a magnitude bit count for negative powers of two.

---

## 2. The two constructors validated nothing

`native_bi_init_string` (`<init>(Ljava/lang/String;)V`) was, in full:

```rust
    let s = ...read_string...;
    bi_write_into(ctx, this, &s);
```

No radix, no length check, no sign check, no digit check. `new BigInteger("abc")`
wrote `"abc"` into the value slot with `signum = 1`; `decimal_to_mag_words`
`continue`s past every non-digit, so the object came out as **signum 1 with an
empty magnitude** — a BigInteger that is simultaneously positive and zero.
`new BigInteger("")` likewise. `new BigInteger("+7")` stored `"+7"`.

`native_bi_init_string_radix` guarded the radix (a previous lane had closed a
real `i128::from_str_radix` **panic** there) and validated the digits — but only
on the `radix != 10` path. Radix 10 fell through `let decimal = if radix == 10 {
s }`, i.e. the operand was passed through as if it were already a decimal
literal. And `<init>(String)` is `this(val, 10)`, so **the unvalidated path was
the common one.**

### The contract, measured

`Ctor.java`, 53 rows. The order of the checks is load-bearing and is the JDK's
(`BigInteger.java:526-552`):

```
new BigInteger((String) null)  !! NullPointerException: Cannot invoke "String.length()" because "val" is null
new BigInteger(null, 40)       !! NullPointerException   <- NOT "Radix out of range": val.length() is the FIRST statement
new BigInteger("", 1)          !! NumberFormatException: Radix out of range      <- radix beats length
new BigInteger("")             !! NumberFormatException: Zero length BigInteger
new BigInteger("-")            !! NumberFormatException: Zero length BigInteger
new BigInteger("5-")           !! NumberFormatException: Illegal embedded sign character
new BigInteger("--5")          !! NumberFormatException: Illegal embedded sign character
new BigInteger("-+5")          !! NumberFormatException: Illegal embedded sign character
new BigInteger("1+2")          !! NumberFormatException: Illegal embedded sign character
new BigInteger("+7")            = 7          new BigInteger("-000") = 0
new BigInteger("1_0")          !! NumberFormatException: For input string: "1_0"
new BigInteger("1234567890123_4567890") !! NumberFormatException: For input string: "3_4567890"
new BigInteger("aaaaaaaaaaaaaaaaaaaaaaaaG", 16) !! NumberFormatException: For input string: "aaaaaaG" under radix 16
```

Two things a source read alone would have got wrong:

* The sign rule is **`lastIndexOf`**, not "starts with". That is why `"5-"` and
  `"1+2"` are *sign* errors and not digit errors.
* The digit message names the **group**, not the operand: the real constructor
  parses in groups of `digitsPerInt[radix]` and reports
  `Integer.parseInt(group, radix)`'s message. `digitsPerInt[10] == 9`, so a
  21-digit operand splits 3 + 9 + 9 and the message names the last nine.

### What landed

One shared `bi_parse_java(s, radix) -> Result<String, MethodCallFailed>` running
the JDK's checks in the JDK's order, plus `bi_number_format_message` (which
carries `BI_DIGITS_PER_INT`, copied verbatim from `BigInteger.java:4795`) and
`bi_ctor_string_arg` (the NPE, ahead of the radix check). Both constructors are
now three lines each over that helper.

### Evidence

`Parity.java` transliterates the new Rust into Java and diffs class + message +
value against the real constructor:

```
PARITY cases=511480 diffs=0
```

Exhaustive over every token of length ≤ 3 from `{0,1,7,9,a,+,-,_,z}` × 14
radices, plus 300,000 fuzzed tokens of length 1..28 over an alphabet that
includes `+ - _ space . : /` and letters on both sides of every radix boundary,
plus 200,000 long digit-only tokens with a planted `_` (which is what exercises
the group split). Zero disagreements on the exception class, the exception
message, and the parsed value.

**RESIDUAL, measured and deliberate.** `Character.digit` accepts non-ASCII
Unicode decimal digits and fullwidth Latin letters; `char::to_digit` is
ASCII-only and Rust's standard library exposes no numeric value for the `Nd`
category:

```
  in=٣        hotspot: OK 3    mine: NumberFormatException: For input string: "٣"
  in=1٣  hotspot: OK 13   mine: NumberFormatException
  in=１２  hotspot: OK 12   mine: NumberFormatException
  in=\uD800        hotspot: NumberFormatException   mine: NumberFormatException   <- agrees
```

This narrows an accepting case; it never widens one. Before this commit those
inputs produced a silent wrong magnitude, which is strictly worse than a
different exception.

---

## 3. The rest of the surface: the sweep for Rust-vs-Java hazards

Six shapes, over both owned files.

| hazard | found | disposition |
|---|---|---|
| integer division / modulo by zero (panics in release) | none in `math_bignum.rs`: every `/` and `%` is either by a literal (10, 256, 2^32) or an `f64` | clean |
| **`as` cast that loses a sign** | **`native_bd_divide_scale`: `prec = new_scale as usize`.** `x.divide(y, -2, HALF_UP)` is a legal BigDecimal call meaning "round to hundreds"; `-2 as usize` is 18_446_744_073_709_551_614 and `format!("{:.prec$}")` then tries to render that many fractional digits. **Unbounded allocation from ordinary bytecode.** | **FIXED** — format at `new_scale.max(0)`, then apply a negative scale through the existing exact `bd_set_scale_impl`. The same edit makes the method read `args[3]`, the **rounding mode, which no code read at all** |
| unchecked shift | the two `BigInteger` shifts (§1) | FIXED |
| `unwrap()` on a fallible value | none reachable; the two in `math_bignum.rs` are `result.last().unwrap()` immediately after a length check, and `char::from_digit(qd, 10).unwrap()` on a `qd < 10` | clean |
| **a `panic!` reachable from Java** | `bi_mod_pow_str` line 746: `panic!("bi_mod_pow_str: negative exponent — caller must compute modInverse first")`. All three call sites (two here, one in `phases_late`) do strip the sign first, so it is guarded today | **left, flagged.** A landmine, not a defect: any future caller that forgets makes it a VM abort |
| **negation overflow** | `-n` on `i32::MIN` in both decimal shift registrations | FIXED (removed with the bodies) |
| **negation overflow, second site** | `bi_shift_left_str(v, i32::MIN)` did `bi_shift_right_str(value, -n)` and `bi_shift_right_str` did the mirror image. `-i32::MIN` panics in debug; in **release it wraps back to `i32::MIN`**, so the two helpers call each other **forever** — a stack overflow, from a sign flip. Neither is registered as a native, so it is unreachable from Java today | **FIXED** — `unsigned_abs`, and the ≥2^31 distance answered directly instead of recursed |

---

## 4. `StringBuilder`: three defects, one of them a twin that won

### (a) `repeat` was registered SIX times for three descriptors

`register_string_builder_natives(registry, class)` is called with three classes
(`lib.rs:18305-18307`). For each it registered `repeat` twice per descriptor:
once spelling the return type `&format!("L{class};")` and once spelling
`Ljava/lang/StringBuilder;` **literally**. For `class ==
"java/lang/StringBuilder"` the two collide and source order decides:

| descriptor | first | second | winner for `StringBuilder` | winner for `StringBuffer` |
|---|---|---|---|---|
| `(Ljava/lang/CharSequence;I)…` | inline closure | `native_sb_repeat_charsequence` | the shared native (harmless) | the shared native |
| `(Ljava/lang/String;I)…` | inline closure | `native_sb_repeat_charsequence` | the shared native (harmless) | the shared native |
| **`(II)…`** | **`native_sb_repeat_codepoint`** | **inline closure** | **the inline closure** | **`native_sb_repeat_codepoint`** |

So the one descriptor where the ordering ran the other way is the one where the
worse body won, **and only for `StringBuilder`**: the same call had two answers
chosen by the receiver's static type. The inline body clamped a negative count
with `.max(0)` (the JDK throws `IllegalArgumentException("count is negative: n")`
— the method's only documented throw) and encoded through `char::from_u32`,
falling back to `code_point as u16`, so `repeat(0x110000, 1)` appended U+0000 and
`repeat(-1, 1)` appended U+FFFF where the JDK refuses both.

This is the fourth instance of this shape in `lang_string.rs` alone. The file
already documents one, on `native_sb_append_code_point`, and closes with:

> the duplicate registration is left in place because removing it would move a
> census count for no behavioural gain.

That reasoning is what let this one survive. **The three duplicate
registrations are now deleted**, so each descriptor has one body across all three
classes.

`native_sb_repeat_charsequence` also had two contract items missing, both
`.max(0)`-shaped silence, both now fixed: the negative-count
`IllegalArgumentException`, and "If `cs` is `null`, then the four characters
`"null"` are repeated into this sequence" (it returned the receiver untouched).

### (b) `String::from_utf16_lossy` on every `String`-returning path

A Rust `str` is well-formed UTF-8 and cannot hold an unpaired surrogate, so
`from_utf16_lossy` replaced every lone `\uD800..\uDFFF` with U+FFFD — silently,
unrecoverably, and **inconsistently with the builder it came from**: after
`sb.append((char) 0xD800)`, `sb.charAt(0)` answered 0xD800 while
`sb.toString().charAt(0)` answered 0xFFFD.

The lossless reader for the other direction, `read_string_chars`, has been in
this file all along with a dozen callers. Its write-side twin did not exist, so
this record adds it: `sb_string_from_units`, which keeps the existing
`create_string_uninterned_gc_safe(&str)` path for well-formed units (so no
allocation path, interning rule or GC-safety property moves for ordinary text)
and only takes `new_object("java/lang/String")` + `init_string_from_units` — the
same pair `native_string_init_from_char_array` uses, documented as preserving
"raw code units byte-for-byte (including unpaired surrogates)" — when
`has_unpaired_surrogate` says the slice actually needs it.

Applied to `toString`, `substring(int)` and `substring(int,int)`.

`indexOf` was the same defect in the read direction, and `[1 of 10 callsites]`
again: `lastIndexOf` already ran on `&[u16]` through `u16_last_index_of`, while
both `indexOf` overloads built a Rust `String` with `from_utf16_lossy` and called
`str::find` — so a lone-surrogate needle was searched for as U+FFFD and could
match a *different* lone surrogate, since they all collapse to the same
replacement character. They now share a new `u16_index_of`, which also fixes a
second bug the rewrite exposed: `sb.indexOf("", 99)` must be the **length**
(`StringUTF16.indexOf`'s first arm, `fromIndex >= valueCount ? (strCount == 0 ?
valueCount : -1)`), and the old body returned -1. The two `lastIndexOf` natives
took the same `read_string_chars` treatment for their needles.

### (c) `reverse()` split surrogate pairs

`AbstractStringBuilder.reverse`'s javadoc is explicit:

> If there are any surrogate pairs included in the sequence, these are treated
> as single characters for the reverse operation. Thus, the order of the
> high-low surrogates is never reversed.

> Note that the reverse operation may result in producing surrogate pairs that
> were unpaired low-surrogates and high-surrogates before the operation.

The body was a plain code-unit swap. It is now `StringUTF16.reverse`'s two-pass
algorithm: reverse every unit, then — only if the input contained a surrogate —
walk the result and swap back each `(low, high)` neighbour, advancing **two**
units after a swap, which is what makes the second sentence of the javadoc true.

### Evidence

`SbParity.java` transliterates all four bodies and diffs them against the real
`StringBuilder`:

```
REVERSE  cases=203906 diffs=0
INDEXOF  cases=200000 diffs=0
REPEATCP cases=126 diffs=0
REPEATCS cases=28 diffs=0
```

`reverse` is exhaustive over every string of length ≤ 5 from
`{a, \uD801, \uDC37, \uD800, \uDC00}` (3,906 strings — every pairing, splitting
and re-pairing the algorithm can meet) plus 200,000 random strings over an
alphabet of 11 units including both lone halves, `￿` and NUL. `indexOf` is
200,000 random (haystack, needle, fromIndex) triples over the same alphabet with
`fromIndex` drawn from `{Integer.MIN_VALUE, -99, -1, 0, …, 99,
Integer.MAX_VALUE}`. `repeat(int,int)` covers all 18 code-point boundaries
(`-1`, `0xD7FF`, `0xD800`, `0xDFFF`, `0xFFFF`, `0x10000`, `0x10FFFF`, `0x110000`,
`Integer.MIN_VALUE`, `Integer.MAX_VALUE`) × 7 counts.

Again: **this proves the constants and the control flow, not that the Rust
compiles.**

---

## 5. What `RJdkBridge1` should do — PREDICTED, every row

The fixture is another lane's and was not touched. It is green on HotSpot
25.0.3+9; the columns below are what this lane's edits should change on CratonVM.

### `bigint` (55 checks)

| row | before | after this lane | note |
|---|---|---|---|
| `(-1).and(5)`, `(-2).or(1)`, `(-1).xor(-1)` | RED, RED, green | **green** | synthetic mode only — real-JDK mode already had the limb bodies |
| `(-1).bitCount()`, `(-9).bitCount()`, `(-1).bitLength()` | RED ×3 | **green** | ditto |
| `ONE.testBit(-1)` → `ArithmeticException` | RED | **green** | ditto |
| `(-1).toByteArray()` | RED | **green** | ditto |
| `zero.not()` | green | green | (`(-1).not()` is not in the fixture and was also wrong) |
| `(-9).shiftRight(1) == -5` | RED | **green** | |
| `16.shiftLeft(-2)`, `16.shiftRight(-2)` | green | green | |
| `ONE.shiftLeft(MIN)` must not throw and must be 0 | RED (answered 1; **panics in a debug build**) | **green** | |
| `ZERO.shiftLeft(MIN)` | green | green | |
| `new BigInteger("")` → NFE | RED | **green** | |
| `new BigInteger("+7") == 7` | RED | **green** | |
| `new BigInteger("1_0")` → NFE | RED | **green** | |
| `new BigInteger("ff",16) == 255` | green | green | |
| `new BigInteger(byte[0])` → NFE | RED | **still RED** | `<init>([B)V` is `phases_late`'s — NOMINATION 2 |
| `new BigInteger(2, byte[]{1})`, `new BigInteger(0, {1})` → NFE | RED ×2 | **still RED** | `<init>(I[B)V` — NOMINATION 2 |
| the other 36 rows | green | green | |

**PREDICTED: `bigint` goes from ~14 failing to 3 failing, and the three left are
all NOMINATION 2.** In real-JDK/compatible mode the shift and constructor rows
are governed by `phases_late` and NOMINATION 1 — `ONE.shiftRight(MIN)` there is
a 256 MB allocation, which the fixture does not exercise but an application can.

### `sbidx` (55 checks)

| row | before | after | note |
|---|---|---|---|
| `repeat(cs, -1)` → `IllegalArgumentException` | RED | **green** | |
| `repeat(cs, 0)`, `repeat("bc", 3)`, `repeat(0x10437, 2).length() == 5` | green | green | |
| `reverse()` keeps a surrogate pair in order (StringBuilder **and** StringBuffer) | RED ×2 | **green** | |
| `reverse()` of an unpaired surrogate | RED | **green** | needed the lossless `toString` too |
| `indexOf("", 99) == 5` | RED | **green** | |
| `indexOf("c", -5) == 2`, `lastIndexOf` rows | green | green | |
| the other ~45 index/exception-class rows | green | green | untouched |

**PREDICTED: `sbidx` goes from ~6 failing to 0 failing.**

### `surrog` (22 checks)

| row | before | after | note |
|---|---|---|---|
| `StringBuilder.insert(lone).toString()` | RED | **green** | lossless `toString` |
| `StringBuilder.indexOf(lone) == 2` | RED | **green** | `u16_index_of` |
| `StringBuilder.reverse(lone LOW surrogate)` | RED | **green** | |
| `StringBuilder.substring(lone)` is one unpaired surrogate | RED | **green** | |
| `new BigInteger("1" + \uD800 + "2")` → NFE | RED | **green** | the constructor now validates digits |
| `append`, `codePointAt`, `codePointCount`, `setCharAt` rows | green | green | already unit-based |
| `Properties`, `TreeMap`, `ArrayDeque`, `Vector`, `URI`, `new String(char[])` rows | unchanged | unchanged | other lanes' files |

**PREDICTED: `surrog` goes from ~5 failing to ~0 failing in this lane's
families**, with the `Properties`/`URI` rows still governed by the census's other
findings.

---

## NOMINATIONS

### 1. `native-builtins/src/phases_late.rs` — the 256 MB `shiftRight`

`register_p71_biginteger_extras` is the winner in real-JDK and compatible mode.
Its left arm has no range guard, so `x.shiftRight(Integer.MIN_VALUE)` runs
`BigInt::shl(2_147_483_648)` → `vec![0u32; 67_108_864]` and then writes 67 M
elements into a Java `int[]`. HotSpot throws `ArithmeticException("BigInteger
would overflow supported range")` for the same call. The helpers to fix it are
already `pub(crate)`-adjacent in `math_bignum.rs`; make them `pub(crate)` and
call them.

OLD (`phases_late.rs`, in `register_p71_biginteger_extras`):

```rust
    r.register(bi, "shiftLeft", "(I)Ljava/math/BigInteger;", |ctx, args| {
        let v = bi_read_int(ctx, obj_arg(args, 0)?);
        let n = match args.get(1) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let res = if n >= 0 {
            v.shl(n as u32)
        } else {
            v.shr(n.unsigned_abs())
        };
        Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &res)?))))
    });
```

NEW:

```rust
    r.register(bi, "shiftLeft", "(I)Ljava/math/BigInteger;", |ctx, args| {
        let v = bi_read_int(ctx, obj_arg(args, 0)?);
        let n = match args.get(1) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        // A left shift past `Integer.MAX_VALUE` magnitude bits is
        // `ArithmeticException("BigInteger would overflow supported range")`
        // (JDK 25 BigInteger.java:1213 `checkRange`), decided from the bit
        // count so the oversized magnitude is never allocated.
        let res = if n >= 0 {
            crate::math_bignum::bi_checked_shl(&v, n as u32)?
        } else {
            v.shr(n.unsigned_abs())
        };
        Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &res)?))))
    });
```

OLD:

```rust
            let res = if n >= 0 {
                v.shr(n as u32)
            } else {
                v.shl(n.unsigned_abs())
            };
            Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &res)?))))
```

NEW:

```rust
            let res = if n >= 0 {
                v.shr(n as u32)
            } else {
                crate::math_bignum::bi_checked_shl(&v, n.unsigned_abs())?
            };
            Ok(Some(Value::Object(Some(bi_alloc_int(ctx, &res)?))))
```

This lane will make `bi_checked_shl` and `bi_mag_bits` `pub(crate)` on request —
they are `fn` today because nothing outside `math_bignum.rs` calls them yet.
**Once this lands, `shiftLeft`/`shiftRight` should be deleted from
`register_biginteger_natives` as well**, finishing the consolidation.

### 2. `native-builtins/src/phases_late.rs` — the two byte-array constructors

`<init>([B)V` calls `bi_from_byte_array_signed(&[])`, which answers `"0"`, where
HotSpot throws `NumberFormatException` for a zero-length array; and `<init>(I[B)V`
accepts any `signum` and does not reject signum 0 with a non-zero magnitude.
MEASURED contract (`RJdkBridge1 bigint`, green on HotSpot):
`new BigInteger(new byte[0])` → `NumberFormatException`;
`new BigInteger(0, new byte[0])` → **0, legal**;
`new BigInteger(2, new byte[]{1})` → `NumberFormatException`;
`new BigInteger(0, new byte[]{1})` → `NumberFormatException`;
`new BigInteger(-1, new byte[]{1})` → -1.
JDK 25 `BigInteger.java:412-460` has the exact messages
(`"Zero length BigInteger"`, `"Invalid signum value"`,
`"signum-magnitude mismatch"`).

### 3. `native-builtins/src/bigint.rs` — `test_bit` allocates 256 MB

```rust
    pub(crate) fn test_bit(&self, n: u32) -> bool {
        let word = (n / 32) as usize;
        let len = self.mag.len().max(word + 1) + 1;
        let tw = self.to_twos(len);
```

`BigInteger.ONE.testBit(Integer.MAX_VALUE)` builds a 67 M-word `Vec<u32>` to read
one bit. HotSpot's `testBit` is `(getInt(n >>> 5) & (1 << (n & 31))) != 0` — O(1),
no allocation. Same denial-of-service shape as NOMINATION 1, reached from a
different method, and it is live in **every** mode. The fix is to answer directly
when `word >= self.mag.len()`: the bit is 0 for a non-negative value and 1 for a
negative one.

### 4. `regression-suite/run.sh`

Seconded from `W8-E29-1`. `RJdkBridge1` still is not registered, and it is the
only instrument that turns any of the above from PREDICTED into MEASURED. Run
`--only=bigint`, `--only=sbidx` and `--only=surrog` in three separate processes
first: `bigint` is the family most likely to abort the VM, and the `-step=`
breadcrumb is what will name the killing call.

---

## Honest summary

Two of the census's five defective families are now fixed in the files this lane
owns, with 916,584 executed comparisons against HotSpot 25.0.3+9 backing the
constants and the control flow of every new body — and **not one line of
CratonVM built or run**, so every "after" above is PREDICTED.

The two most interesting findings were not the ones the vector was aimed at.
First, the census's `unsigned_abs` sentence was wrong in all three of its claims,
and the real defect — a missing `checkRange` on the left-shift arm — sits in a
*different registrar*, which is also where it is still live. Second, the
mechanism behind six of the ten `BigInteger` rows and the whole `repeat` row is
the same one: **a correct implementation exists, a later registration shadows
it, and `register()` is last-wins.** Eleven registrations were deleted in this
commit and nothing was written to replace them. That is the only move in this
defect class that has ever reduced it.
