# G10-1 — the bignum surface measured, and the twin that is not compiled at all

**Status: CODE LANDED, BEHAVIOUR UNVERIFIED ON CRATONVM.** 2026-08-16, lane
G10. The only source files this lane edited are
`native-builtins/src/math_bignum.rs` and `native-builtins/src/bigint.rs` (plus
this record). `native-builtins/src/biginteger_intrinsics.rs` was audited and
**deliberately left unchanged** — §7.

> **Nothing in this record was measured on a CratonVM binary.** This lane did
> not build, did not run the suite, and did not use `--dump-native-registry`
> (the orchestrator owns the build and was running the regression suite for the
> whole of this session). Every claim is labelled:
>
> * **MEASURED** — observed on the oracle, Temurin `openjdk 25.0.3 2026-04-21
>   LTS (25.0.3+9-LTS)`, transcript quoted. Probes:
>   `scratchpad/g10/{BdProbe,Bd2Probe,BiProbe}.java`.
> * **SOURCE-VERIFIED** — read out of this working tree at
>   `claude/jdk-only-mode-completion-1351c0`, with file and line.
> * **PREDICTED** — a statement about what CratonVM will do. Falsifiable by one
>   run. Treat as a hypothesis.
>
> Per HANDOFF §5: *a green build proves you broke nothing, not that you did
> something.* Nothing below is proved until §11's checks run.

Line numbers in this record are as of the end of this lane's edits and will
drift; the anchors that do not drift are the function names. A line number
proves where a body is, never that it runs (HANDOFF §5).

---

> **VERIFIED AGAINST A BINARY 2026-09-04. 670 of 671 checks agree; the one that
> does not is a real defect.** This record's status was **CODE LANDED, BEHAVIOUR
> UNVERIFIED ON CRATONVM** — *"This lane did not build, did not run the suite,
> and did not use `--dump-native-registry`."*
>
> **`RJdkBigNum` is not in the regression suite.** This record writes the vector
> out in full and nobody ever added it, so a suite run could never have
> exercised it — `regression-suite/src/RJdkBigNum.java` does not exist. The
> source was extracted from §11 of this record and is now kept as
> `probes/RJdkBigNum.java` so the next reader does not have to.
>
> ```text
>                       checks   differing from HotSpot   self-diff over 2 runs
> HotSpot 25              671      —                        0
> CratonVM compatible     671      1                        0
> CratonVM --jdk-only     671      1                        0
> ```
>
> **671 is exactly the count this record predicted** (*"exit 0, 671 checks,
> under two seconds"*), so the vector is the one it describes and nothing has
> drifted underneath it.
>
> **The single divergence, in the `doubleValue()` surface of §5.2:**
>
> ```text
> dv.9007199254740993.1     HotSpot 9.007199254740992E14     CratonVM 9.007199254740993E14
> ```
>
> `new BigDecimal(BigInteger.valueOf(9007199254740993L), 1).doubleValue()` is
> the decimal 900719925474099.3, and 9007199254740993 is 2^53 + 1 — the first
> integer a `double` cannot represent. HotSpot returns the correctly-rounded
> nearest `double` and prints `…992`; we print `…993`, a value no `double`
> holds. The unrounded digits surviving is the tell: the conversion is not going
> through an IEEE-754 rounding step. It is the only row of the 84
> `dv.`/`fv.` combinations that differs, and it is deterministic across runs on
> both VMs, so it is a defect and not a flake.
>
> **What this does NOT verify.** §7's deliberate decision to leave
> `native-builtins/src/biginteger_intrinsics.rs` unchanged — *"the twin that is
> not compiled at all"* — is untouched: this vector cannot see a file that is
> not compiled, and a green here is not evidence about it. The MEASURED HotSpot
> transcripts from `scratchpad/g10/{BdProbe,Bd2Probe,BiProbe}.java` are the
> oracle and were not re-derived; that scratchpad did not survive its session.
> The defect above is named, NOT localised to a function.

## 1. Verdict

| | |
|---|---|
| the brief's census claim — "synthetic-only registrar with a shipping twin, last-write-wins" | **CORRECTED, and it matters** (§2). Under `--jdk-only` the synthetic registrars are not *compiled*, not merely outvoted; and the shipping `BigInteger` registrar is itself partly shadowed by a **third** registrar in a file this lane does not own |
| F20-1 / F31-1's `setScale` boundary work | **RE-VERIFIED against a fresh sweep**, 8 rounding modes × 13 scales + the zero axis + the rounding table: **every row already agreed** (§4). No edit needed; the record can close |
| a scale that overflows into a panic, still live | **`native_bd_multiply`'s `sa + sb`** — found, measured, fixed (§5.1). Shipping registrar, no twin |
| the argument-driven ~2 GB render F31-1 §10 nominated | **closed on the one caller that ships**, `doubleValue` (§5.2) |
| F31-1 N3's `panic!` in `bi_mod_pow_str` | **closed** (§5.3) |
| a second `debug_assert`-only panic guard, not previously recorded | `BigInt::divmod_mag`'s zero divisor (§5.4) |
| fabricated success in a shipping body | **`BigInteger.compareTo(null)` answered `0`** (§5.5) |
| reading another class's field slots as a magnitude | **`BigInteger.equals(Object)` had no `instanceof`** (§5.5) |
| `biginteger_intrinsics.rs` | audited, **no VM-abort path found**, unchanged (§7) |
| records that can close | E38-1, F20-1, F31-1 partially — §9 |
| NOMINATIONS raised | **6** (§10) |

---

## 2. Which body actually wins under `--jdk-only`

This is the section to read if you read nothing else, because the brief's
premise is wrong in a way that changes what is worth fixing.

### 2.1 The synthetic registrars are not outvoted — they are not compiled

**SOURCE-VERIFIED.**

`register_biginteger_natives` (`math_bignum.rs:1379`) and
`register_bigdecimal_natives` (`math_bignum.rs:3443`) have exactly one caller
each, `native-builtins/src/lib.rs:24176-24177`, inside
`register_synthetic_overrides` (`lib.rs:21780`), which carries
`#[cfg(feature = "synthetic-jdk")]` at `lib.rs:21779`.
`native-builtins/Cargo.toml` sets `default = []`, and no crate in the workspace
turns `synthetic-jdk` on by default (`vm/Cargo.toml:30` states it explicitly).

So in the shipping build those two functions are **dead code, never called**.
There is no last-write-wins race between them and anything: in `--jdk-only`
they are absent, and in a `--features synthetic-jdk` build they run second and
win. The brief's framing — *"any test built with `--features synthetic-jdk`
measures the copy that does not ship"* — is right about the consequence and
wrong about the mechanism, and the mechanism is the part that tells you where
to spend effort.

### 2.2 `BigDecimal` has ONE shipping registrar, and it is in this lane's file

**SOURCE-VERIFIED.** A repo-wide scan for `"java/math/BigDecimal"` as a
registration class finds exactly two binding sites, both in `math_bignum.rs`:
line 1292 (`register_bigdecimal_arithmetic_overrides`, declared 1287) and line
3447 (`register_bigdecimal_natives`, declared 3443 — the dead one). `phases_late.rs` registers no
`BigDecimal` triple at all.

`register_bigdecimal_arithmetic_overrides` is called unconditionally from
`register_essential_natives_with_shims` (`lib.rs:7190`) at `lib.rs:7357`, and
registers **19 triples** (enumerated mechanically from the function body, not by eye):

```
add  subtract  multiply  negate  signum  scale  precision
valueOf(J)  valueOf(D)  toString  toPlainString
intValue  longValue  doubleValue
setScale(I)  setScale(II)  toBigInteger
<init>(D)  <init>(Ljava/math/BigInteger;)
```

Everything else on `BigDecimal` — **`divide` in every overload, `compareTo`,
`equals`, `hashCode`, `abs`, `floatValue`, `stripTrailingZeros`,
`<init>(String)`, `<init>(I)`, `<init>(J)`, `toBigIntegerExact`, `round`,
`movePointLeft/Right`, `scaleByPowerOfTen`, `toEngineeringString`,
`intValueExact` and friends** — has **no native** in `--jdk-only` and runs real
JDK bytecode.

That single fact retires most of the divergence list in the older records:
`native_bd_divide`'s `f64` round-trip, `native_bd_compare_to`'s `f64` tie at 17
significant digits, `native_bd_hash_code`'s string hash and
`native_bd_strip_zeros` are all **unreachable under `--jdk-only`**. They are
live only in synthetic-JDK mode. §8 keeps their measured oracle rows so the
next lane does not have to re-measure, but this lane deliberately spent nothing
on them.

### 2.3 `BigInteger` has THREE registrars, and the last one is not in this file

**SOURCE-VERIFIED.** Inside the single function
`register_essential_natives_with_shims`, in statement order:

| # | line | registrar | file | category |
|---|---|---|---|---|
| 1 | `lib.rs:7356` | `register_biginteger_arithmetic_overrides` | `math_bignum.rs:1207` | `Intrinsic` |
| 2 | `lib.rs:8000` | `phases_late::register_p71_biginteger_extras` | `phases_late.rs:8483` | `Bridge` |

(`biginteger_intrinsics::register_biginteger_intrinsics`, `lib.rs:10182`, is a
third but disjoint: it binds only the JDK-private `implSquareToLen` /
`shiftLeftImplWorker` / `shiftRightImplWorker` / `implMulAdd` / `mulAdd`.)

Both are unconditional top-level statements in the same function, so **#2 runs
after #1**. `NativeMethodRegistry::register` is last-write-wins
(`native-api/src/registry.rs:6057` → `register_inner`), and the `--jdk-only`
gate at the top of `register_inner` drops a registration only when
`effective_category().allowed_in(JdkOnly)` is false, which is
`!matches!(self, NativeKind::SyntheticStub)` — so **both `Intrinsic` and
`Bridge` register in `--jdk-only`**. `retired_shadow` lists no `java/math/*`
triple.

Consequence — the shipping owner of each `BigInteger` triple:

| triple | winner under `--jdk-only` |
|---|---|
| `add`, `subtract`, `multiply` | **`phases_late`** (registered later; this file's copies are shadowed) |
| `negate`, `signum`, `toString()`, `intValue`, `longValue`, `pow`, `valueOf(J)`, `compareTo(BigInteger)`, `compareTo(Object)`, `equals` | **`math_bignum.rs`** (this lane's file) |
| `gcd`, `isProbablePrime`, `shiftLeft`, `shiftRight`, `and`, `or`, `xor`, `not`, `testBit`, `bitLength`, `bitCount`, `toByteArray`, `<init>([B)`, `<init>(I[B)`, `modPow`, `modInverse`, `mod`, `remainder`, `divide`, `intValueExact`, `longValueExact` | **`phases_late`** (this file registers none of them any more — E38-1's consolidation) |
| `abs`, `max`, `min`, `hashCode`, `doubleValue`, `floatValue`, `toString(int)`, `<init>(String)`, `<init>(String,int)` | **nobody** — real JDK bytecode |

**PREDICTED.** Every fix in §5 that this lane made to a `BigInteger` native is
in the third row of that table — `compareTo`, `equals` — i.e. in triples this
file still owns. This lane made **no** edit to `add`/`subtract`/`multiply` on
`BigInteger`, precisely because `phases_late` owns them.

`bigint.rs` is a different case and is worth stating: it is a **library**, not
a registrar. `phases_late`'s winning bodies call into it (`bi_read_int` →
`crate::bigint::BigInt`), so a panic in `bigint.rs` is a VM abort reachable
through the shipping path even though `bigint.rs` registers nothing. That is
why §5.4 is in scope.

### 2.4 How this was established, and what would settle it properly

By reading `#[cfg]` attributes, `Cargo.toml` feature tables, call-site line
numbers within one function, and `register_inner`'s gate — all cited above.
**This is SOURCE-VERIFIED, not MEASURED.** HANDOFF §4 is explicit that only
`--dump-native-registry` settles it (`owns_slot=true` plus non-zero
`invocations`). §11 asks the orchestrator to run exactly that.

---

## 3. What the oracle says — `BigInteger`

**MEASURED**, `scratchpad/g10/BiProbe.java`, Temurin 25.0.3+9. Labels are ASCII
throughout (HANDOFF §7).

### 3.1 Shifts at and beyond `Integer.MIN_VALUE`

The rule is one sentence and it is *not* symmetric: a negative distance flips
direction and is then read **unsigned**, so `shiftLeft(MIN)` is a right shift by
2^31 (which succeeds, clearing everything) while `shiftRight(MIN)` is a left
shift by 2^31 (which refuses).

```text
bi(1).shiftLeft(-2147483648)   = 0
bi(1).shiftRight(-2147483648) !! ArithmeticException: BigInteger would overflow supported range
bi(1).shiftLeft(2147483647)   !! ArithmeticException: BigInteger would overflow supported range
bi(1).shiftRight(2147483647)   = 0
bi(-1).shiftLeft(-2147483648)  = -1        bi(-1).shiftRight(2147483647) = -1
bi(-2).shiftLeft(-2147483647)  = -1        bi(-3).shiftRight(-33) = -25769803776
bi(0).shiftLeft(2147483647)    = 0         bi(0).shiftRight(-2147483648) = 0
```

Zero is exempt on every row, at every distance, in both directions.

### 3.2 `mod` vs `remainder` vs `divide`, and the zero divisor

```text
bi(-7).mod(3)        = 2      bi(-7).remainder(3)  = -1     bi(-7).divide(3)  = -2
bi(7).mod(-3)       !! ArithmeticException: BigInteger: modulus not positive
bi(7).remainder(-3)  = 1      bi(7).divide(-3)     = -2
bi(1).divide(ZERO)  !! ArithmeticException: BigInteger divide by zero
bi(0).divide(ZERO)  !! ArithmeticException: BigInteger divide by zero
bi(1).mod(ZERO)     !! ArithmeticException: BigInteger: modulus not positive
bi(1).remainder(ZERO) !! ArithmeticException: BigInteger divide by zero
bi(-4).sqrt()       !! ArithmeticException: Negative BigInteger
bi(-1).gcd(bi(0))    = 1      bi(0).gcd(bi(0))     = 0
```

Three different messages for three flavours of "zero on the right", and `mod`'s
is about the *modulus* being non-positive, not about division.

### 3.3 `modPow` / `modInverse` with non-positive moduli

```text
bi(3).modPow(2, 0)   !! ArithmeticException: BigInteger: modulus not positive
bi(3).modPow(2, -7)  !! ArithmeticException: BigInteger: modulus not positive
bi(3).modPow(-1, 7)   = 5          bi(-3).modPow(3, 7)  = 1      bi(3).modPow(2, 1) = 0
bi(2).modPow(-1, 4)  !! ArithmeticException: BigInteger not invertible.
bi(3).modInverse(0)  !! ArithmeticException: BigInteger: modulus not positive
bi(2).modInverse(4)  !! ArithmeticException: BigInteger not invertible.
bi(3).modInverse(1)   = 0          bi(-3).modInverse(7) = 2
```

Note the trailing period in `"BigInteger not invertible."` and its absence in
`"BigInteger: modulus not positive"`.

### 3.4 Constructors and their refusals

```text
new BigInteger((String)null) !! NullPointerException: Cannot invoke "String.length()" because "val" is null
new BigInteger("")           !! NumberFormatException: Zero length BigInteger
new BigInteger("", 1)        !! NumberFormatException: Radix out of range     (radix beats length)
new BigInteger("-")          !! NumberFormatException: Zero length BigInteger
new BigInteger("5-")         !! NumberFormatException: Illegal embedded sign character
new BigInteger("1_0")        !! NumberFormatException: For input string: "1_0"
new BigInteger("ff", 10)     !! NumberFormatException: For input string: "ff"
new BigInteger("+7") = 7     new BigInteger("-000") = 0     new BigInteger("z", 36) = 35
new BigInteger("1", 1) / ("1", 37) / ("1", MIN) !! NumberFormatException: Radix out of range
new BigInteger(new byte[0])  !! NumberFormatException: Zero length BigInteger
new BigInteger((byte[])null) !! NullPointerException: Cannot read the array length because "val" is null
new BigInteger(new byte[]{-1})    = -1        new BigInteger(new byte[]{0,-1}) = 255
new BigInteger(0, new byte[0])    = 0         new BigInteger(1, new byte[0])   = 0
new BigInteger(0, new byte[]{1})  !! NumberFormatException: signum-magnitude mismatch
new BigInteger(2, new byte[]{1})  !! NumberFormatException: Invalid signum value
new BigInteger(1, (byte[])null)   !! NullPointerException: Cannot read the array length because "magnitude" is null
```

None of these constructors is registered by this file under `--jdk-only`
(§2.3); `<init>([B)` and `<init>(I[B)` belong to `phases_late`.

### 3.5 `toString(radix)` at the bounds — the row most likely to be guessed wrong

```text
bi(255).toString(-2147483648) = 255      bi(255).toString(0)  = 255
bi(255).toString(-1)          = 255      bi(255).toString(1)  = 255
bi(255).toString(37)          = 255      bi(255).toString(MAX)= 255
bi(255).toString(2)  = 11111111    bi(255).toString(16) = ff    bi(255).toString(36) = 73
bi(-255).toString(16) = -ff        bi(0).toString(37)   = 0
```

**An out-of-range radix silently falls back to 10. It does not throw.** No
native is bound to `toString(I)` in `--jdk-only`, so this is informational —
but the twin in `register_biginteger_natives` (`native_bi_to_string_radix`)
implements the same rule, and any lane tempted to "add validation" there would
be adding a divergence.

### 3.6 Bit surface, exact narrowings, identity

```text
bi(1).testBit(-1) / testBit(MIN) !! ArithmeticException: Negative bit address
bi(1).testBit(MAX)  = false    bi(-1).testBit(MAX) = true
bi(1).setBit(-1) / flipBit(-1)   !! ArithmeticException: Negative bit address
bi(0).bitLength() = 0     bi(-1).bitLength() = 0    bi(-9).bitLength() = 4
bi(0).bitCount()  = 0     bi(-1).bitCount()  = 0    bi(-9).bitCount()  = 1
bi(0).getLowestSetBit() = -1                        bi(-9).getLowestSetBit() = 0
bi(2147483648).intValueExact()  !! ArithmeticException: BigInteger out of int range
bi(9223372036854775808).longValueExact() !! ArithmeticException: BigInteger out of long range
bi(128).byteValueExact()        !! ArithmeticException: BigInteger out of byte range
bi(1).equals(null) = false   bi(1).equals("1") = false   bi(1).equals(Long.valueOf(1)) = false
bi(1).compareTo(null)      !! NullPointerException: Cannot read field "signum" because "val" is null
bi(1).max(null)            !! NullPointerException: Cannot read field "signum" because "val" is null
((Comparable) bi(1)).compareTo("1") !! ClassCastException: class java.lang.String cannot be cast to
    class java.math.BigInteger (java.lang.String and java.math.BigInteger are in module java.base
    of loader 'bootstrap')
TWO.pow(-1)  !! ArithmeticException: Negative exponent      ZERO.pow(0)  = 1
ZERO.pow(-1) !! ArithmeticException: Negative exponent      ONE.pow(MAX) = 1
TEN.pow(715827883) !! ArithmeticException: BigInteger would overflow supported range
TWO.pow(MAX)       !! ArithmeticException: BigInteger would overflow supported range
bi(-1).pow(MAX) = -1
bi(-7).isProbablePrime(10) = true    bi(0).isProbablePrime(0) = true
bi(1000003).multiply(bi(1000033)).isProbablePrime(20) = false
```

The last two rows re-confirm F7's and `phases_late`'s current bodies; nothing
here contradicts them.

---

## 4. What the oracle says — `BigDecimal`

**MEASURED**, `scratchpad/g10/{BdProbe,Bd2Probe}.java`.

### 4.1 `setScale` — 8 rounding modes × 13 scales, and every row already agreed

The full cross-product was run. The interesting columns are identical for all
eight modes:

```text
setScale(1.5, <any mode>, -2147483648) !! ArithmeticException: Underflow
setScale(1.5, <any mode>, -2147483647) !! ArithmeticException: Underflow
setScale(1.5, <any mode>, -715827884)  !! ArithmeticException: BigInteger would overflow supported range
setScale(1.5, <any mode>, -715827883)  !! ArithmeticException: BigInteger would overflow supported range
setScale(1.5, <any mode>,  715827883)  !! OutOfMemoryError: Java heap space     (it really tries)
setScale(1.5, <any mode>,  2147483646) !! ArithmeticException: BigInteger would overflow supported range
setScale(1.5, <any mode>,  2147483647) !! ArithmeticException: BigInteger would overflow supported range
setScale(1.5, UP, -3) = 0E+3   setScale(1.5, UP, -1) = 1E+1   setScale(1.5, UP, 5) = 1.50000
```

`bd_set_scale_impl` (`math_bignum.rs`) reproduces every one of these:
`|diff| > i32::MAX` → `bd_underflow()`; otherwise `bd_pow_ten_check(raise|drop)`,
whose boundary is exactly `n >= 715_827_883`. **SOURCE-VERIFIED against the
transcript, no edit required.** F20-1 and F31-1 were right.

The zero axis is exempt at both extremes, also already implemented:

```text
ZERO.setScale(MIN, HALF_UP) = 0E+2147483648    bd(0,3).setScale(MIN) = 0E+2147483648
ZERO.setScale(MAX, HALF_UP) = 0E-2147483647    bd(0,3).setScale(MAX) = 0E-2147483647
```

The rounding-mode argument is validated **before** the same-scale and
zero-value short-circuits — the one ordering fact a plausible implementation
gets wrong:

```text
1.5.setScale(0, -1) / (0,8) / (0,99) / (0,MIN) / (0,MAX) !! IllegalArgumentException: Invalid rounding mode
1.5.setScale(0, 0) = 2      1.5.setScale(0, 7) !! ArithmeticException: Rounding necessary
bd(15,1).setScale(1, 99)  !! IllegalArgumentException: Invalid rounding mode   (same scale, still refuses)
bd(0,1).setScale(1, 99)   !! IllegalArgumentException: Invalid rounding mode   (zero value, still refuses)
```

`bd_set_scale_impl` checks the mode as its first statement. Already correct.

The rounding table, all 8 modes × 11 signed values, reproduces
`bd_round_needs_increment` exactly, including the three rows that separate the
HALF_* family:

```text
UP         2.5->3  -2.5->-3  1.5->2  -1.5->-2  0.5->1  -0.5->-1  2.4->3  2.6->3
DOWN       2.5->2  -2.5->-2  1.5->1  -1.5->-1  0.5->0  -0.5->0   2.4->2  2.6->2
CEILING    2.5->3  -2.5->-2  1.5->2  -1.5->-1  0.5->1  -0.5->0   2.4->3  2.6->3
FLOOR      2.5->2  -2.5->-3  1.5->1  -1.5->-2  0.5->0  -0.5->-1  2.4->2  2.6->2
HALF_UP    2.5->3  -2.5->-3  1.5->2  -1.5->-2  0.5->1  -0.5->-1  2.4->2  2.6->3
HALF_DOWN  2.5->2  -2.5->-2  1.5->1  -1.5->-1  0.5->0  -0.5->0   2.4->2  2.6->3
HALF_EVEN  2.5->2  -2.5->-2  1.5->2  -1.5->-2  0.5->0  -0.5->0   2.4->2  2.6->3
UNNECESSARY  every non-exact row !! ArithmeticException      0.0 -> 0
```

`-0.5` under DOWN, CEILING and HALF_DOWN is `0`, never `-0`.

### 4.2 `toString` vs `toPlainString` vs `toEngineeringString` — the differences are the point

```text
              toString      toPlainString            toEngineeringString
bd(0,-1)      0E+1          0                        0.00E+3
bd(0,5)       0.00000       0.00000                  0.00000
bd(0,-5)      0E+5          0                        0.0E+6
bd(1,6)       0.000001      0.000001                 0.000001
bd(1,7)       1E-7          0.0000001                100E-9
bd(1,-1)      1E+1          10                       10
bd(1,-6)      1E+6          1000000                  1E+6
bd(123,-5)    1.23E+7       12300000                 12.3E+6
bd(-123,-5)   -1.23E+7      -12300000                -12.3E+6
bd(10,3)      0.010         0.010                    0.010
bd(1000,3)    1.000         1.000                    1.000
bd(12,-20)    1.2E+21       1200000000000000000000   1.2E+21
```

`bd_layout_chars` (`toString`) matches **every** row above — the `scale == 0`
short-circuit, the `scale > 0 && adjusted >= -6` plain arm, the scientific arm's
`+` sign for a non-negative adjusted exponent. **SOURCE-VERIFIED, no edit.**
`toEngineeringString` has no native and runs JDK bytecode.

### 4.3 `toBigInteger` / `intValue` / `longValue` at the scale extremes

```text
bd(1,MIN).toBigInteger()   !! ArithmeticException: Underflow
bd(1,MIN+1).toBigInteger() !! ArithmeticException: BigInteger would overflow supported range
bd(1,MAX).toBigInteger()   !! ArithmeticException: BigInteger would overflow supported range
bd(0,MIN).toBigInteger()    = 0
bd(1,MIN).intValue() = 0    bd(1,MIN).longValue() = 0    bd(1,MAX).intValue() = 0
bd(1,MIN).precision() = 1   bd(0,MIN).scale() = -2147483648
bd(1,MIN).negate()          = unscaled -1, scale -2147483648
```

Two different messages one scale apart, and `intValue`/`longValue` refusing
nothing. `bd_to_big_integer_check` + `bd_narrowing_truncates_to_zero` reproduce
all of it. F31-1 was right.

### 4.4 `divide` — measured, and **not registered under `--jdk-only`**

```text
1.divide(3)          !! ArithmeticException: Non-terminating decimal expansion; no exact representable decimal result.
1.divide(0)          !! ArithmeticException: Division by zero
0.divide(0)          !! ArithmeticException: Division undefined
1.divide(2)           = 0.5        1.divide(2,HALF_UP)  = 1
1.divide(3,5,HALF_UP) = 0.33333    1.divide(3,-1,HALF_UP) = 0E+1
1.divide(3,5,99)     !! IllegalArgumentException: Invalid rounding mode
1.divide(3,(RoundingMode)null) !! NullPointerException: Cannot read field "oldMode" because "roundingMode" is null
1.divide((BigDecimal)null)     !! NullPointerException: Cannot invoke "java.math.BigDecimal.signum()" because "divisor" is null
1.divideToIntegralValue(0)     !! ArithmeticException: Division by zero
1.remainder(0)                 !! ArithmeticException: Division by zero
```

`native_bd_divide` says `"BigDecimal divide by zero"` — a string that appears
nowhere in the JDK. **It does not ship** (§2.2). Recorded, not fixed. §10 N5.

### 4.5 The rest of the family, measured once so nobody re-measures it

```text
1.23.movePointLeft(MIN)      !! ArithmeticException: BigInteger would overflow supported range
1.23.movePointRight(MIN)     !! ArithmeticException: Underflow
1.23.movePointLeft(MAX)      !! ArithmeticException: Underflow
1.23.movePointRight(MAX)     !! ArithmeticException: BigInteger would overflow supported range
1.23.scaleByPowerOfTen(MIN)  !! ArithmeticException: Underflow
1.23.scaleByPowerOfTen(MAX)   = 1.23E+2147483647       ZERO.movePointLeft(MIN) = 0

strip("600.0")=6E+2 scale=-2    strip("100")=1E+2 scale=-2    strip("-0.00")=0 scale=0
strip("0.000")=0 scale=0        strip("1E-10")=1E-10 scale=10 strip(bd(0,MIN))=0 scale=0

2.00.intValueExact() = 2        2.01.intValueExact() !! ArithmeticException: Rounding necessary
2147483648.intValueExact()     !! ArithmeticException: Overflow
128.byteValueExact()           !! ArithmeticException: Overflow
2.5.toBigIntegerExact()        !! ArithmeticException: Rounding necessary

2.0.equals(2.00) = false   2.0.compareTo(2.00) = 0
2.0.hashCode() = 621       2.00.hashCode() = 6202
2.0.equals(null) = false   2.0.equals("2.0") = false
2.0.compareTo(null)        !! NullPointerException: Cannot read field "scale" because "val" is null

new BigDecimal((String)null)     !! NullPointerException: Cannot invoke "String.toCharArray()" because "val" is null
new BigDecimal("")               !! NumberFormatException   (message is NULL, not "")
new BigDecimal(" 1")             !! NumberFormatException: Character   is neither a decimal digit number,
                                    decimal point, nor "e" notation exponential mark.
new BigDecimal("1E-2147483648")  !! NumberFormatException: Exponent overflow.
new BigDecimal((BigInteger)null) !! NullPointerException: Cannot invoke "Object.getClass()" because "val" is null
new BigDecimal(Double.NaN)       !! NumberFormatException: Infinite or NaN
new BigDecimal(0.1) = 0.1000000000000000055511151231257827021181583404541015625
new BigDecimal(-0.0) = 0         BigDecimal.valueOf(-0.0) = 0.0
BigDecimal.valueOf(1e300) = 1.0E+300   BigDecimal.valueOf(1L,MIN) = 1E+2147483648
123.456.round(new MathContext(2)) = 1.2E+2      new MathContext(-1) !! IllegalArgumentException: Digits < 0
```

`new BigDecimal("")` throwing a `NumberFormatException` whose `getMessage()` is
**`null`** is the kind of row that cannot be derived. It is transcribed here and
nowhere used, because `<init>(String)` has no native under `--jdk-only`.

---

## 5. The fixes

Each one names the registrar that wins, and how that was established.

### 5.1 `native_bd_multiply` — an `i32` scale addition that panics in debug and wraps in release

**Registrar: `register_bigdecimal_arithmetic_overrides`, `math_bignum.rs:1287`.
No twin exists (§2.2), so this body wins under `--jdk-only`
(SOURCE-VERIFIED).** Highest-priority class: a Rust panic is a VM abort where
HotSpot throws.

Before:

```rust
    let s = sa + sb;
```

`sa` and `sb` are two caller-chosen `BigDecimal` scales. `bd(1,MAX).multiply(bd(1,1))`
overflows that `i32`. `[profile.release]` in the workspace `Cargo.toml` does not
set `overflow-checks`, so release **wraps silently** and writes a large negative
scale into the result — which then renders ~2 GB of trailing zeros the first
time anything calls `toPlainString()` on it — while a debug/`livedbg` build
**panics**, and a panic in a native is not catchable from Java.

After: `bd_product_scale`, transcribing `BigDecimal.checkScale`. The refusal is
decided by the **receiver**'s zeroness, which is the one part that has to be
transcribed rather than derived (`bd(1,MAX).multiply(bd(0,MAX))` throws even
though the product is zero). MEASURED table in §5.1 of the source doc comment
and reproduced in the unit test `multiply_product_scale_matches_hotspot`.

**PREDICTED after:** `bd(1,MAX).multiply(bd(1,MAX))` → `ArithmeticException:
Underflow`; `bd(1,MIN).multiply(bd(1,MIN))` → `ArithmeticException: Overflow`;
`bd(0,MAX).multiply(bd(1,MAX))` → unscaled 0, scale `Integer.MAX_VALUE`.

### 5.2 `native_bd_double_value` — a 2 GB render for a value HotSpot answers in 0 ms

**Registrar: same as 5.1. `doubleValue` IS in the shipping list; `floatValue`
is NOT** (§2.2) — that asymmetry is the whole reason to state the registrar per
fix.

Before, both were `bd_read_unchecked(ctx, this).parse()`. `bd_read_unchecked` is
`apply_scale`, which for a negative scale appends `scale.unsigned_abs()` literal
`'0'` characters. `bd(1, Integer.MIN_VALUE).doubleValue()` therefore asked for a
2_147_483_649-byte `String` from one ordinary call. HotSpot answers `Infinity`
in 0 ms and never renders anything. This is F31-1 §10's first residual; it
cannot be closed by refusing, only by not rendering.

After: `bd_to_f64` / `bd_to_f32` compute from `(unscaled, scale)` directly. The
rendering that remains is `0.<digits>E<e10>` — bounded by the **operand's**
digit count, never by the argument — and Rust's parser is correctly rounded, the
same round-to-nearest-even the JDK's own
`Double.parseDouble(this.toString())` fall-through performs. The two clamps are
proved, not tuned: `adjusted >= 309` is `> f64::MAX`, `adjusted <= -325` is below
half the smallest subnormal. `f32`'s are 39 / -46.

MEASURED and pinned in `double_value_matches_hotspot` /
`float_value_matches_hotspot`:

```text
bd(1,MIN)=Infinity  bd(-1,MIN)=-Infinity  bd(0,MIN)=0.0   bd(1,-309)=Infinity  bd(1,-308)=1.0E308
bd(1,MAX)=0.0       bd(-1,MAX)=-0.0       bd(1,323)=9.9E-324  bd(1,324)=0.0    bd(15,324)=1.5E-323
bd(49,326)=0.0      bd(-1,324)=-0.0       bd(9007199254740993,0)=9.007199254740992E15
bd(17976931348623157,-292)=1.7976931348623157E308   bd(17976931348623159,-292)=Infinity
floatValue: bd(1,-308)=Infinity (its doubleValue is 1.0E308); bd(-1,308)=-0.0;
            bd(9007199254740993,1)=9.0071994E14
```

`bd(1,324)` rounding to zero while `bd(15,324)` does not — same adjusted
exponent, different answers — is why the low clamp is at `-325` and not `-324`.
A zero value is `+0.0` at every scale, never `-0.0`.

### 5.3 `bi_mod_pow_str` — the `panic!` F31-1 N3 nominated

**Reachable through `phases_late::register_p71_biginteger_extras`'s `modPow`
(the shipping owner, §2.3) and through this file's synthetic `modPow`.** Both
call sites strip the sign first, so it was a landmine and not a live defect —
but a `pub(crate)` helper whose own comment says it chose to "panic to be loud"
is one careless caller from a VM abort no `catch` can see.

`bi_mod_pow_str_opt` is now the real body: a negative exponent inverts the base
(`bi_mod_inverse_str`, thirty lines down the same file — the JDK's own rule,
`BigInteger.java:2564-2566`) and returns `None` for the one shape a `String`
cannot express, a negative exponent over a non-invertible base.
`bi_mod_pow_str` keeps the infallible signature its out-of-lane caller needs and
maps `None` to `"0"`.

**`"0"` there is a wrong value, and it is deliberately preferred to a VM
abort.** No in-tree caller can reach it. §10 N1 nominates
`phases_late.rs:8847` onto the `_opt` form so the wrong value becomes
unreachable by construction rather than by inspection.

### 5.4 `BigInt::divmod_mag` — a second `debug_assert`-only guard, not previously recorded

`bigint.rs`. `debug_assert!(n > 0, "divmod_mag: zero divisor")` is compiled out
of `--release`, and the next use of `n` is `v[n - 1]`: `0usize - 1` wraps and
the slice index **panics**. All four public wrappers (`div`, `rem`, `divmod`,
`modulo`) short-circuit `o.is_zero()` first, so it is a landmine. Removed for
the cost of one comparison, on a path that already trims both operands.

`bigint.rs` registers nothing, but `phases_late`'s winning `BigInteger` bodies
compute on it (§2.3), so this is on the shipping path.

### 5.5 `BigInteger.compareTo` / `equals` — fabricated success and a foreign field read

**Registrar: `register_biginteger_arithmetic_overrides`, `math_bignum.rs:1207`.
`phases_late` registers neither `compareTo` nor `equals`, so this file owns all
three descriptors** (`compareTo(BigInteger)`, `compareTo(Object)`,
`equals(Object)`) **under `--jdk-only`** (SOURCE-VERIFIED).

* `compareTo(null)` answered **`0`** — "these two are equal". A `TreeMap` or a
  `Collections.sort` over a list with one null silently produced an ordering.
  HotSpot: `NullPointerException: Cannot read field "signum" because "val" is
  null` (MEASURED; transcribed, not derived).
* `equals(Object)` had no `instanceof` screen and read **any** argument through
  `BigInteger`'s `signum` + `mag` slots. `BigInteger.ONE.equals("1")` compared a
  `String`'s field slots against a magnitude. HotSpot: `false` (MEASURED).
* `compareTo(Object)` with a non-`BigInteger` now raises `ClassCastException`.
  The measured HotSpot message carries a module/loader parenthetical built by
  `vm/src/runtime/exceptions.rs:1363`, which this crate cannot reach; the
  message here is the class-correct prefix only. That is **class (e), wrong
  message text**, replacing **class (b), a fabricated comparison**. §10 N2.

The type screen is two `ClassId` reads on the fast path (`BigInteger` is
`final`, so an exact class match is the whole subtype test) and resolves a class
name only when the ids already differ — `compareTo` is on BouncyCastle's
field-arithmetic and Lucene's `TestUtil.nextLong` hot paths and must not pay a
`String` per call.

**Risk the orchestrator must weigh (§11).** This turns a lenient `0` into a
throw. `register_biginteger_arithmetic_overrides`'s own header says
`BigInteger.<clinit>` can fail in real-JDK mode and that a post-clinit fixup
populates the static constants. If any bootstrap path compares against a
not-yet-populated constant, it previously got `0` and now gets an NPE. One-line
revert if so: restore `_ => return Ok(Some(Value::Int(0)))` for the
`Object(None)` arm.

### 5.6 `native_bd_init_bigint` — right exception, wrong text

**Registrar: shipping (`<init>(Ljava/math/BigInteger;)V` is in the list).**
`new BigDecimal((BigInteger) null)` threw a message-less NPE. HotSpot's message
is `Cannot invoke "Object.getClass()" because "val" is null` (MEASURED).
Transcribed. Class (e).

---

## 6. Tests added

All in existing `#[cfg(test)]` modules, none new.

`math_bignum.rs`, module `argument_driven_range_tests`:

* `multiply_product_scale_matches_hotspot` — the §5.1 table, both words
  (`Underflow` / `Overflow`) and the zero-receiver exemption.
* `double_value_matches_hotspot`, `float_value_matches_hotspot` — the §5.2
  tables, including `-0.0` sign checks via `is_sign_negative()`.
* **`every_rescale_path_is_total_at_the_scale_extremes`** — the no-panic pin the
  brief asked for. 7 scales × 5 operands, and 7×7 scale pairs for the binary
  paths, driving `bd_to_big_integer_check` + `bd_truncate`,
  `bd_narrowing_truncates_to_zero`, `bd_plain_string_check`, `bd_to_f64`,
  `bd_to_f32`, `bd_product_scale` and `bd_rescale_operand`. It asserts nothing
  about the *values*: it asserts every call **returns**. Those are the four
  shapes that have each been a live abort in this family — F20's `-scale`,
  F31's `new_scale - scale` and `s - sa`, and G10's `sa + sb`.
* `mod_pow_negative_exponent_no_longer_panics` — §5.3, both the `_opt` form's
  `None` and the wrapper's non-aborting `"0"`.

`bigint.rs`, module `tests`:

* `division_by_zero_returns_instead_of_panicking` — §5.4, across `div`, `rem`,
  `modulo`, `divmod` and `modpow`.

`rustfmt --edition 2021 --check` parses all three owned files (the pre-existing
whole-file diff is import ordering that predates this lane; **none of the code
this lane wrote appears in it**). Zero CR bytes in all three. No duplicate `fn`
names.

---

## 7. `biginteger_intrinsics.rs` — audited, deliberately unchanged

It is the **sole** registrar for `implSquareToLen`, `shiftLeftImplWorker`,
`shiftRightImplWorker`, `implMulAdd`, `mulAdd` (`lib.rs:10182`), so it always
wins. Every native entry point validates its length and index arguments against
the real array lengths before calling the algorithm helpers, and the helpers'
own `debug_assert!`s are backed by real guards (`mul_add` and `add_one` both
return early on an out-of-range offset; `primitive_left_shift_inplace` masks the
shift count with `& 31` so a debug-build shift overflow cannot happen).

**SOURCE-VERIFIED: no panic path, no unguarded slice index, no
`abs()`/negation of a caller-chosen `i32`.** Its only `panic!`/`expect(` sites
are inside `#[cfg(test)]`. Changing it would have been change for its own sake.

---

## 8. What this lane did NOT do

* **It did not build, run, or measure CratonVM.** Every "after" is PREDICTED.
  Nothing here has been compared against a binary.
* **It did not use `--dump-native-registry`.** §2's ownership table is
  SOURCE-VERIFIED — `#[cfg]` attributes, `Cargo.toml` feature tables, statement
  order inside one function, and `register_inner`'s gate. HANDOFF §4 says only
  the dump settles it. §11 asks for it.
* **It did not touch `phases_late.rs`**, which owns `BigInteger`'s
  `add`/`subtract`/`multiply`/shifts/bit-ops/`modPow`/`divide` under
  `--jdk-only`. Everything found there is a NOMINATION.
* **It did not fix the `BigDecimal` natives that do not ship** —
  `native_bd_divide`, `native_bd_divide_scale`, `native_bd_compare_to`,
  `native_bd_equals`, `native_bd_hash_code`, `native_bd_strip_zeros`,
  `native_bd_abs`, `native_bd_init_string/int/long`. All still route through
  `f64` or a string hash, all still render through `bd_read_unchecked`, and all
  are measurably wrong against §4.4/§4.5 — **in synthetic-JDK mode only.**
  Fixing them would have been the exact waste the brief warns about. Their
  oracle rows are in §4 so the next lane starts from measurement.
* **It did not add non-ASCII to any label** (HANDOFF §7). Every probe label is
  ASCII; the em-dashes in this document are prose, not printed output.
* **It did not touch `regression-suite/`.** The vector below is a code block,
  not a file.
* **It did not attempt the `f64` precision axis on `compareTo`.** F31-1 §10
  flagged it; it does not ship; a blind rewrite is a worse trade than a recorded
  residual.
* **It did not port `toString(radix)`'s silent radix-10 fallback anywhere**, or
  "fix" it. §3.5 is informational.

---

## 9. Records that can close, and on what evidence

| record | disposition |
|---|---|
| **F20-1** | N1 (three unguarded rescales) and N2 (`bd_read` must refuse) were closed by F31-1 and are **re-verified against a fresh 8×13 sweep** (§4.1). N3 is not this lane's. F20-1's own `apply_scale` claim stands. **Closable**, with the caveat that "closable" here means SOURCE-VERIFIED + oracle-agreeing, not measured on CratonVM. |
| **F31-1** | §3/§4/§5 (the three roads, the zero exemption, the `bd_read` split) all re-verified (§4.1, §4.3). **N3 closed** (§5.3). **§10's first residual closed on the one caller that ships** (§5.2); the other eleven callers are unreachable under `--jdk-only` (§2.2), which downgrades that residual from "a live DoS" to "a synthetic-mode DoS". **Mostly closable.** |
| **E38-1** | Its registration-consolidation note (`math_bignum.rs:1660-1700`) is **confirmed correct in every particular** — including the `else`-arm claim that this file's synthetic registrar never runs in real-JDK mode, which §2.1 strengthens (it is not compiled). Its `BigInteger` ctor/shift analysis now lives in `phases_late`. **Closable as to this file.** |
| **W8-F7-1** | The argument-driven allocation sweep: `pow`, `testBit`, `shiftLeft/Right`, `bd_pow_ten_check` all still hold (§3.1, §3.6). The one shape it did **not** cover — a *rendering* driven by the argument rather than an allocation — is §5.2. **Amended, not closed.** |
| **W7-44** | Untouched by this lane. `NumberFormat`/enum/`Double.toString` is a different registrar family; the only contact point is `native_bd_value_of_double`, which asks Java for `Double.toString` rather than reproducing it, and whose measured rows (§4.5) agree. **No change.** |

---

## 10. NOMINATIONS

Six, all outside this lane's three files.

### N1 — `native-builtins/src/phases_late.rs:8847`: take the fallible `modPow` helper

`bi_mod_pow_str_opt` now exists (`math_bignum.rs`). The call site already
computes the inverse itself, so it always passes a non-negative exponent and the
`None` arm is unreachable today — but switching makes it unreachable *by
construction* and lets the wrapper's `"0"` fallback (§5.3) be deleted.

*change:* `let res = bi_mod_pow_str(&inv, pos_exp, &m);` →
```rust
let res = match crate::math_bignum::bi_mod_pow_str_opt(&inv, pos_exp, &m) {
    Some(v) => v,
    None => {
        return Err(RuntimeError::ArithmeticException {
            message: "BigInteger not invertible.".to_string(),
        }
        .into());
    }
};
```

### N2 — `native-api` (or a new shared helper): the canonical `ClassCastException` message

`vm/src/runtime/exceptions.rs:1363` builds the full JDK form
(`class X cannot be cast to class Y (<module/loader parenthetical>)`).
`native-builtins` cannot reach it — the dependency runs the other way — so
`math_bignum.rs`'s new `compareTo(Object)` arm and
`native-collections/src/lib.rs:17971` / `:43096` each emit the prefix only.
*change:* hoist the builder to `native-api` (or expose it through
`NativeContext`) and route all three sites through it.

### N3 — `native-builtins/src/phases_late.rs`, `register_p71_biginteger_extras`: `mod` vs `remainder` messages

MEASURED (§3.2): `mod(ZERO)` and `mod(-3)` are
`ArithmeticException: BigInteger: modulus not positive`, while `remainder(ZERO)`
and `divide(ZERO)` are `ArithmeticException: BigInteger divide by zero`. This
lane did not read those bodies closely enough to assert they diverge — it is a
**nomination to check**, with the oracle rows supplied.

### N4 — `native-builtins/src/phases_late.rs`: `BigInteger.compareTo`/`equals` are NOT registered there

Stated as a nomination *not* to act: a future lane adding them to `p71` would
silently take ownership from `math_bignum.rs` (later registration wins, §2.3)
and revert §5.5. If that is ever wanted, move the bodies, do not duplicate them.

### N5 — `native-builtins/src/math_bignum.rs` (this lane's own file, deliberately deferred): the synthetic-only `BigDecimal` natives

`native_bd_divide`'s `"BigDecimal divide by zero"` is not a JDK string (§4.4);
`native_bd_compare_to`, `native_bd_divide*`, `native_bd_hash_code` and
`native_bd_strip_zeros` are all `f64`- or string-based. They are **unreachable
under `--jdk-only`**, so this lane spent nothing on them rather than risk
landing correct code in a body with `invocations = 0`. Raised as a nomination so
the deferral is a decision and not an oversight.

### N6 — `vm-cli` / orchestrator: make the two modes' divergence visible

The `--jdk-only` and `--features synthetic-jdk` builds run **different
`BigInteger`/`BigDecimal` code** (§2.1). Nothing in the tree makes that visible
at test time. *change:* have the census dump flag any triple whose owning
registrar differs between the two feature configurations, or gate the
synthetic-only registrars behind a boot-time assertion that they never own a
slot in `--jdk-only`.

---

## 11. What the orchestrator must check at build time

1. **`cargo check`/`build` first.** The signature of `bi_mod_pow_str` is
   unchanged (deliberately), but `bi_mod_pow_str_opt` is new and `bigint.rs`'s
   test module gained a test. `native-builtins` depends on `native-io`
   (HANDOFF §7): if `native-io` fails, nothing here is checked at all.
2. **`--dump-native-registry` on any vector that touches `java.math`.** §2's
   whole ownership table is SOURCE-VERIFIED and unmeasured. What must be true:
   * `java/math/BigDecimal.multiply(...)`, `.doubleValue()`,
     `.setScale(II)`, `<init>(Ljava/math/BigInteger;)V` →
     `registered_by` ends `math_bignum.rs`, `owns_slot=true`.
   * `java/math/BigInteger.compareTo(Ljava/math/BigInteger;)I` and
     `.equals(Ljava/lang/Object;)Z` → `owns_slot=true`, `math_bignum.rs`.
   * `java/math/BigInteger.add(...)` → `owns_slot=true`, **`phases_late.rs`**,
     with `overwrote=Some(Intrinsic)`. If this one says `math_bignum.rs`, §2.3
     is wrong and the whole ownership table needs re-deriving.
   * Any row with `invocations = 0` that this lane claims to have fixed is a
     wasted fix — say so.
3. **Watch for a new `NullPointerException` during bootstrap.** §5.5 turns
   `BigInteger.compareTo(null)` from `0` into a throw. The revert is one line
   and is named in §5.5.
4. **Do not run the `setScale(…, 715827883)` rows anywhere timed.** HotSpot
   spends 60-90 s per row before `OutOfMemoryError: Java heap space` (§4.1) —
   that is the *correct* behaviour and CratonVM should match it, so any
   regression vector must avoid the row rather than assert on it.
5. The `every_rescale_path_is_total_at_the_scale_extremes` test is the one that
   must never be deleted. If it starts failing with a *panic* rather than an
   assertion, that is a VM-abort regression, not a test bug.

---

## 12. Regression vector

Not created as a file (`regression-suite/` is out of bounds for this lane).
`RNumbers` already exists and is scheduled; these rows belong there or in a new
`RJdkBigNum`. Every expectation is MEASURED on Temurin 25.0.3+9. Labels are
ASCII; the harness compares CratonVM's stdout against HotSpot's, so nothing
printed may be encoding-dependent.

**This block was compiled and run on the oracle as written**: single-file source
mode, `java -Xmx512m RJdkBigNum.java`, exit 0, **671 checks**, under two seconds,
two `setScale(int,int)` deprecation warnings and nothing else. It deliberately
omits the `715827883` scales (section 11.4) so it stays fast. Output is
deterministic: no randomness, no timing, no `hashCode` of an identity.

```java
import java.math.BigDecimal;
import java.math.BigInteger;
import java.math.RoundingMode;

public class RJdkBigNum {
    static int checks = 0;

    static void guarded(String label, java.util.function.Supplier<Object> s) {
        checks++;
        String out;
        try {
            Object o = s.get();
            out = (o == null) ? "<null>" : String.valueOf(o);
        } catch (Throwable t) {
            out = "!! " + t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + " = " + out);
    }

    static BigDecimal bd(long u, int s) {
        return new BigDecimal(BigInteger.valueOf(u), s);
    }

    public static void main(String[] args) {
        final int MIN = Integer.MIN_VALUE;
        final int MAX = Integer.MAX_VALUE;

        // --- multiply: the product scale is checkScale(scale1 + scale2), and
        //     the RECEIVER's zeroness is what exempts. G10-1 section 5.1.
        guarded("mul.1", () -> bd(1, MAX).multiply(bd(1, MAX)));
        guarded("mul.2", () -> bd(1, MAX).multiply(bd(1, 1)));
        guarded("mul.3", () -> bd(1, MAX).multiply(bd(0, MAX)));
        guarded("mul.4", () -> bd(0, MAX).multiply(bd(1, MAX)).scale());
        guarded("mul.5", () -> bd(1, MIN).multiply(bd(1, MIN)));
        guarded("mul.6", () -> bd(1, MIN).multiply(bd(1, -1)));
        guarded("mul.7", () -> bd(0, MIN).multiply(bd(1, MIN)).scale());
        guarded("mul.8", () -> bd(1, MAX).multiply(bd(1, MIN)).scale());
        guarded("mul.9", () -> bd(1, 1073741824).multiply(bd(1, 1073741824)));

        // --- doubleValue / floatValue must not render the scale. Every one of
        //     these answers in 0 ms on HotSpot. G10-1 section 5.2.
        int[] ds = { MIN, MIN + 1, -400, -309, -308, -1, 0, 1, 308, 309, 323, 324, 325, MAX };
        long[] us = { 1L, -1L, 0L, 15L, -15L, 9007199254740993L };
        for (long u : us) {
            for (int s : ds) {
                guarded("dv." + u + "." + s, () -> bd(u, s).doubleValue());
                guarded("fv." + u + "." + s, () -> bd(u, s).floatValue());
            }
        }
        guarded("dv.max", () -> bd(17976931348623157L, -292).doubleValue());
        guarded("dv.inf", () -> bd(17976931348623159L, -292).doubleValue());

        // --- setScale over every RoundingMode at the scales that overflow when
        //     negated. Deliberately EXCLUDES 715827883, which HotSpot spends
        //     ~90 s on before OutOfMemoryError. G10-1 section 4.1 / section 11.4.
        for (RoundingMode rm : RoundingMode.values()) {
            for (int sc : new int[] { MIN, MIN + 1, -715827884, -715827883, -3, -1, 0, 2, MAX - 1, MAX }) {
                guarded("ss." + rm + "." + sc, () -> new BigDecimal("1.5").setScale(sc, rm));
                guarded("ss0." + rm + "." + sc, () -> BigDecimal.ZERO.setScale(sc, rm));
            }
        }
        for (int m : new int[] { -1, 0, 7, 8, 99, MIN, MAX }) {
            guarded("ssm." + m, () -> new BigDecimal("1.5").setScale(0, m));
            guarded("ssm0." + m, () -> bd(0, 1).setScale(1, m));
        }

        // --- the rounding table, all 8 modes.
        String[] halves = { "2.5", "-2.5", "1.5", "-1.5", "0.5", "-0.5", "2.4", "-2.4", "2.6", "-2.6", "0.0" };
        for (RoundingMode rm : RoundingMode.values()) {
            for (String v : halves) {
                guarded("rt." + rm + "." + v, () -> new BigDecimal(v).setScale(0, rm));
            }
        }

        // --- toBigInteger / intValue / longValue / toPlainString at the extremes.
        guarded("tbi.1", () -> bd(1, MIN).toBigInteger());
        guarded("tbi.2", () -> bd(1, MIN + 1).toBigInteger());
        guarded("tbi.3", () -> bd(1, MAX).toBigInteger());
        guarded("tbi.4", () -> bd(0, MIN).toBigInteger());
        guarded("iv.1", () -> bd(1, MIN).intValue());
        guarded("lv.1", () -> bd(1, MIN).longValue());
        guarded("tps.1", () -> bd(1, MIN).toPlainString());
        guarded("tps.2", () -> bd(1, MAX).toPlainString());

        // --- toString's three roads. toPlainString and toEngineeringString
        //     differ from toString and from each other, on purpose.
        long[][] tv = { {0,0},{0,1},{0,-1},{0,5},{0,-5},{1,0},{1,6},{1,7},{1,-1},{1,-6},
                        {123,5},{123,-5},{-123,-5},{10,3},{100,3},{1000,3},{12,-20} };
        for (long[] r : tv) {
            final long u = r[0]; final int s = (int) r[1];
            guarded("ts." + u + "." + s, () -> bd(u, s).toString());
            guarded("tp." + u + "." + s, () -> bd(u, s).toPlainString());
            guarded("te." + u + "." + s, () -> bd(u, s).toEngineeringString());
        }
        guarded("neg.1", () -> bd(1, MIN).negate().scale());
        guarded("prec.1", () -> bd(1, MIN).precision());
        guarded("bdnull", () -> new BigDecimal((BigInteger) null));

        // --- BigInteger identity: null and wrong-typed arguments.
        //     G10-1 section 5.5.
        guarded("bi.eq.null", () -> BigInteger.ONE.equals(null));
        guarded("bi.eq.str", () -> BigInteger.ONE.equals("1"));
        guarded("bi.eq.int", () -> BigInteger.ONE.equals(Integer.valueOf(1)));
        guarded("bi.eq.self", () -> BigInteger.ONE.equals(BigInteger.valueOf(1)));
        guarded("bi.cmp.null", () -> BigInteger.ONE.compareTo(null));
        guarded("bi.max.null", () -> BigInteger.ONE.max(null));

        // --- BigInteger shifts: a negative distance flips direction and is
        //     then read UNSIGNED. G10-1 section 3.1.
        String[] sv = { "0", "1", "-1", "-2", "3", "-3" };
        int[] sh = { 0, 1, 31, 32, 33, -1, -31, -32, -33, MIN, MIN + 1, MAX };
        for (String v : sv) {
            for (int n : sh) {
                guarded("shl." + v + "." + n, () -> summarize(new BigInteger(v).shiftLeft(n)));
                guarded("shr." + v + "." + n, () -> summarize(new BigInteger(v).shiftRight(n)));
            }
        }

        // --- mod / remainder / divide / modPow / modInverse refusals.
        guarded("bi.divz", () -> BigInteger.ONE.divide(BigInteger.ZERO));
        guarded("bi.remz", () -> BigInteger.ONE.remainder(BigInteger.ZERO));
        guarded("bi.modz", () -> BigInteger.ONE.mod(BigInteger.ZERO));
        guarded("bi.modneg", () -> new BigInteger("7").mod(new BigInteger("-3")));
        guarded("bi.mod.sign", () -> new BigInteger("-7").mod(new BigInteger("3")));
        guarded("bi.rem.sign", () -> new BigInteger("-7").remainder(new BigInteger("3")));
        guarded("bi.mp.0", () -> new BigInteger("3").modPow(new BigInteger("2"), BigInteger.ZERO));
        guarded("bi.mp.neg", () -> new BigInteger("3").modPow(new BigInteger("-1"), new BigInteger("7")));
        guarded("bi.mp.ninv", () -> new BigInteger("2").modPow(new BigInteger("-1"), new BigInteger("4")));
        guarded("bi.mi.0", () -> new BigInteger("3").modInverse(BigInteger.ZERO));
        guarded("bi.mi.ninv", () -> new BigInteger("2").modInverse(new BigInteger("4")));
        guarded("bi.pow.neg", () -> BigInteger.TWO.pow(-1));
        guarded("bi.pow.zero", () -> BigInteger.ZERO.pow(0));
        guarded("bi.pow.one", () -> BigInteger.ONE.pow(MAX));
        guarded("bi.sqrt.neg", () -> new BigInteger("-4").sqrt());
        guarded("bi.tb.neg", () -> BigInteger.ONE.testBit(-1));
        guarded("bi.tb.max", () -> BigInteger.ONE.testBit(MAX));
        guarded("bi.tb.max.neg", () -> BigInteger.valueOf(-1).testBit(MAX));

        System.out.println("RJdkBigNum checks=" + checks);
    }

    static String summarize(BigInteger b) {
        String s = b.toString();
        if (s.length() > 60) {
            return "<signum=" + b.signum() + " bitLength=" + b.bitLength() + ">";
        }
        return s;
    }
}
```

---

## 13. Things that are true and easy to disbelieve

* **`BigInteger.toString(-2147483648)` is `"255"`, not an exception** (§3.5).
  Every out-of-range radix silently becomes 10.
* **`bd(1,MAX).multiply(bd(0,MAX))` throws while `bd(0,MAX).multiply(bd(1,MAX))`
  does not** (§5.1). The product is zero either way; `checkScale` is called on
  the receiver.
* **`bd(1,324).doubleValue()` is `0.0` but `bd(15,324).doubleValue()` is
  `1.5E-323`** (§5.2), and they share an adjusted exponent. Any clamp written at
  `-324` gets the second one wrong.
* **`bd(0,1).setScale(1, 99)` throws** (§4.1) — same scale, zero value, and the
  rounding-mode check still fires, because it is the method's first statement.
* **HotSpot spends 60-90 seconds on `1.5.setScale(715827883)` before
  `OutOfMemoryError`** (§4.1). A cap anywhere below `715827883` refuses where
  HotSpot succeeds; F20-1 established this and a fresh sweep re-confirms it.
* **`new BigDecimal("")` throws a `NumberFormatException` whose message is
  `null`** (§4.5) — distinct from `""`, and not derivable.
* **The two build modes run different `java.math` code** (§2.1). A green
  `--features synthetic-jdk` test run says nothing about `--jdk-only`.
