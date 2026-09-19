# W8-F7-1 — one bit that cost 256 MB, and the sweep for everything else sized by an argument

**Status: LANDED in `native-builtins/src/bigint.rs` and
`native-builtins/src/math_bignum.rs` (the two files this lane owns), plus four
NOMINATIONS in files it does not, and one cross-lane duplicate resolved from
this side. This lane cannot build or run CratonVM: every statement about VM
behaviour is SOURCE-READ or PREDICTED. Every statement about HotSpot is
MEASURED on Microsoft OpenJDK 25.0.3+9, with the transcript quoted.**

Follow-up to `E38-1-biginteger-shifts-ctors-and-the-stringbuilder-repeat-twin.md`,
whose NOMINATION 3 is the headline here. Probes:
`scratchpad/f7/{TestBit,Pow,PowParity,Scale,Shr,ByteArr,Prime}.java`.

The vector was a **shape**, not a method: *an allocation or a loop whose size is
driven by an ARGUMENT rather than by the operands' actual magnitude.* Four live
instances were found. Three are in files this lane owns and are fixed; the
fourth is stated with its boundary unmeasured, because HotSpot does not refuse
it either.

---

## 1. `BigInt::test_bit` allocated ~256 MB to read one bit — in every mode

```rust
    pub(crate) fn test_bit(&self, n: u32) -> bool {
        let word = (n / 32) as usize;
        let len = self.mag.len().max(word + 1) + 1;
        let tw = self.to_twos(len);            // <- vec![0u32; len]
        (tw[word] >> (n % 32)) & 1 == 1
    }
```

`BigInteger.ONE.testBit(Integer.MAX_VALUE)` is `vec![0u32; 67_108_866]`, ~256 MB,
to read a bit whose value is a function of the **sign alone**. It is reachable
from one line of ordinary bytecode, it is registered by
`phases_late::register_p71_biginteger_extras` so it is live in **every** jdk
mode, and it is also on the `BigDecimal` rounding path
(`bd_round_needs_increment` calls `quotient.test_bit(0)`).

HotSpot's is O(1) (`BigInteger.java:3747`):

```java
    public boolean testBit(int n) {
        if (n < 0) throw new ArithmeticException("Negative bit address");
        return (getInt(n >>> 5) & (1 << (n & 0x1f))) != 0;
    }
```

MEASURED (`TestBit.java`):

```
ONE.testBit(Integer.MAX_VALUE) = false   [0 ms]
(-1).testBit(Integer.MAX_VALUE) = true   [0 ms]
ZERO.testBit(Integer.MAX_VALUE) = false   [0 ms]
ONE.testBit(-1) = !! java.lang.ArithmeticException: Negative bit address   [4 ms]
(2^64).testBit(0)      = false        (-(2^64)).testBit(0)  = false
(-(2^64)).testBit(64)  = true         (-(2^64)).testBit(65) = true
(-(2^64+1)).testBit(0) = true         (-(2^64+1)).testBit(1) = true
```

### The sign rule is not "index into the magnitude"

`getInt` (`BigInteger.java:4838`) is the whole contract:

```java
        if (n >= mag.length) return signInt();          // 0, or -1 when negative
        int magInt = mag[mag.length-n-1];
        return (signum >= 0 ? magInt :
                (n <= numberOfTrailingZeroInts() ? -magInt : ~magInt));
```

For a negative value the words **at or below** the lowest non-zero limb are
negated and the ones above it are complemented — the borrow out of the low words
has already been consumed. That is why the four `2^64` rows above are in the
probe: `-(2^64)` has limbs `[0, 0, 1]`, so limb 0 goes through `-magInt` (which
is 0) and limb 2 through `-magInt` as well, while `-(2^64+1)` moves the lowest
non-zero limb to index 0 and changes the answer for limbs 1 and 2. A body that
just indexes the magnitude and flips for negatives gets all four wrong.

### What landed

`BigInt::get_int` (new, private, allocation-free) plus a two-line `test_bit` over
it. `to_twos` stays — `bitop` and `bit_count` still need it, and their length is
driven by the OPERAND, which is the distinction this whole record is about.

### Evidence

`TestBit.java` transliterates the new body **and the old one** into Java and
diffs both against `java.math.BigInteger`:

```
TESTBIT cases=371547 diffs(new)=0 diffs(old)=0 old-skipped(too big to materialise)=6237
```

76 operands (both signs, every word boundary, values with interior and low zero
limbs, 400 random values up to 300 bits, each also shifted left by up to 200 to
plant zero limbs) × 417 bit indices (0..400 exhaustive, plus 511/512/513, 2^16,
2^20, 2^24, 2^26-1, 2^26, 2^30, `MAX_VALUE-32`, `MAX_VALUE-1`, `MAX_VALUE`).

**The old body was CORRECT** — `diffs(old)=0` — it was only ruinously expensive.
That is the finding worth carrying: this defect class does not show up as a
wrong answer, so a differential harness that only compares answers scores it
green. The 6,237 skipped rows are the ones the old body could not be *run* on
inside this probe, which is the measurement.

---

## 2. `pow` had no range guard at all, and it is registered in BOTH modes

`native_bi_pow` refused a negative exponent and then went straight into
square-and-multiply. `BigInteger.TEN.pow(1_000_000_000)` is one line of ordinary
bytecode asking for a 3.3-billion-bit number, and this native starts building
it. It is registered from `register_biginteger_arithmetic_overrides`
(`lib.rs:7269`, essentials — every mode) as well as from
`register_biginteger_natives`, so unlike the shift family there is no mode where
something else answers.

HotSpot bounds the answer from the operand's bit length and the exponent and
throws **before** it starts. MEASURED (`Pow.java`):

```
ZERO.pow(0) = 1   [0 ms]                    ZERO.pow(MAX) = 0   [0 ms]
ONE.pow(MAX) = 1   [0 ms]                   (-1).pow(MAX) = -1   [0 ms]
(-1).pow(MAX-1) = 1   [0 ms]                TWO.pow(-1) !! ArithmeticException: Negative exponent
TWO.pow(MAX)   !! ArithmeticException: BigInteger would overflow supported range   [94 ms]
TWO.pow(MAX-1)  = <signum=1 bitLength=2147483647>   [36 ms]
TWO.pow(1<<30)  = <signum=1 bitLength=1073741825>   [50 ms]
THREE.pow(MAX) !! …overflow…   [0 ms]       THREE.pow(1<<20) = <bitLength=1661954>   [335 ms]
TEN.pow(MAX)   !! …overflow…   [0 ms]       TEN.pow(1000000000) !! …overflow…   [0 ms]
(2^100).pow(MAX)   !! …overflow…   [0 ms]   (2^100).pow(1<<26) !! …overflow…   [0 ms]
(-2).pow(MAX)  !! …overflow…   [68 ms]      (2^31-1).pow(MAX)  !! …overflow…   [0 ms]
TEN.pow(3) = 1000    (-2).pow(3) = -8    (-2).pow(4) = 16
```

Note the three timings that are **not** 0 ms. They are the rows where HotSpot's
guard passes and its *inner* `shiftLeft` refuses after allocating — the same
allocate-then-refuse shape E38-1 found on `shiftRight`. The predicate has three
separate arms and they are not interchangeable.

### What landed

`bi_pow_check_range`, a port of the JDK's **refusal only**
(`BigInteger.java:2594-2650`); the exponentiation stays the existing
square-and-multiply, which is mathematically identical to the JDK's repeated
squaring. The JDK factors `2^powersOfTwo` out of the base and shifts it back at
the end, so its guard bounds `(remainingBits - 1)·exponent + bitsToShift + 1` —
exactly the magnitude bit length of the answer the existing loop builds. That is
what makes the two accept-sets the same rather than merely similar. The trivial
rows (`exponent == 0 || this.equals(ONE)` → ONE; `signum == 0 || exponent == 1`
→ this) are answered first, which is why `ZERO.pow(MAX)` and `ONE.pow(MAX)` cost
nothing on either side.

### Evidence

`PowParity.java` transliterates the whole new native and diffs class, message
and value against `java.math.BigInteger.pow`:

```
POW PARITY cases=2560 diffs=0 value-compared=1635 skipped(answer>2Mbit on both sides)=284
```

80 bases (both signs, powers of two, near-word-boundary values, 2^100, 30 random
values up to 400 bits) × 36 exponents (`MIN_VALUE`, -1, 0, 1, the word
boundaries, 2^20, 2^24, 2^26, 2^28, 2^30, 2e9, `MAX-1`, `MAX`). The 284 skipped
rows are the ones this model ACCEPTS whose answer exceeds 2 Mbit — legitimately
expensive on both sides, and therefore not a divergence.

---

## 3. `setScale` subtracted two `i32` scales and then asked for 2 GB of `'0'`

```rust
    if new_scale >= scale {
        let padded = bigint_mul_pow10(&unscaled, new_scale - scale);
        …
    }
    let drop = (scale - new_scale) as usize;
    divisor_dec.push_str(&"0".repeat(drop));
```

Two defects in three lines, both driven by the argument:

* **`i32` overflow.** `new_scale - scale` and `scale - new_scale` are plain
  `i32` subtractions of two caller-chosen scales. `new BigDecimal("1.5")
  .setScale(Integer.MIN_VALUE)` overflows: a **panic** in a debug build — and a
  panic is not a Java throwable, it takes the VM down and cannot be caught — and
  a silent wrap in release.
* **the allocation.** After the wrap, `"0".repeat(drop)` asks for up to 2 GB.

The JDK computes the difference in a `long` and that is the entirety of
`checkScale`. MEASURED (`Scale.java`):

```
1.5.setScale(MAX_VALUE, HALF_UP) !! ArithmeticException: BigInteger would overflow supported range  [15 ms]
1.5.setScale(MIN_VALUE, HALF_UP) !! ArithmeticException: Underflow   [0 ms]
1.5.setScale(-2, HALF_UP) = 0E+2     1.5.setScale(0, HALF_UP) = 2     1.5.setScale(5) = 1.50000
1.5.setScale(0) !! ArithmeticException: Rounding necessary
0.setScale(MAX_VALUE) = 0E-2147483647     0.setScale(MIN_VALUE) = 0E+2147483648
1.5.setScale(1000000) = <1000002 chars, scale=1000000>   [2950 ms]
```

Three rules come out of those rows, and all three are now in the code:

1. **A zero unscaled value takes ANY scale and never consults `checkScale`** —
   the two `0.setScale(...)` rows. This is `zeroValueOf(newScale)`, ahead of
   everything.
2. **An out-of-range difference is `ArithmeticException("Underflow")`**, never
   "Overflow": both of `checkScale`'s call sites pass the POSITIVE magnitude
   (`newScale - oldScale` when raising, `oldScale - newScale` when dropping), so
   the clamp is always `Integer.MAX_VALUE` and `asInt > 0` always picks the
   first message.
3. **An in-range but enormous difference still refuses**, because
   `BigDecimal.bigTenToThe(n)` is `BigInteger.TEN.pow(n)` for anything past its
   table — so it inherits §2's guard exactly. `bd_pow_ten_check` calls
   `bi_pow_check_range` with base 10 rather than inventing a cap, which is why
   `setScale(MAX_VALUE)` refuses and `setScale(1000000)` (2950 ms on HotSpot!)
   does not.

Nothing else in the method moved: the zero and equal-scale shortcuts produce the
same object the old arithmetic produced, so the only behaviour change is the two
refusals.

---

## 4. The sweep: what was checked and cleared

| candidate | verdict |
|---|---|
| `BigInt::test_bit` | **FIXED** (§1) |
| `native_bi_pow` | **FIXED** (§2) |
| `bd_set_scale_impl` | **FIXED** (§3) |
| `bi_shift_right_str`, positive `n`, NEGATIVE value | **FIXED.** The `if q == "0" { break }` guard fires only for a non-negative value: the negative arm computes `ceildiv(q, 2)`, whose fixpoint is **1**, not 0. So `bi_shift_right_str("-1", Integer.MAX_VALUE)` ran 2^31 arbitrary-precision decimal divisions to return `-1`, which the sign-extension arm below already knew. Not registered as a native, so this is a landmine rather than a live defect — E38-1 fixed this helper's `i32::MIN` mutual recursion and left the loop |
| `bi_shift_left_str`, positive `n` | **cleared, with a caveat.** The `for _ in 0..n` doubling loop IS argument-driven, but so is the output: `v << n` has `bitlen(v) + n` bits, so there is no cheaper answer to give. It is O(n²) where HotSpot is O(n). Not registered; not a divergence |
| `bi_test_bit_str` | **clear.** The `for _ in 0..n` loop halves `w` and breaks at `"0"`, so it is bounded by `bitlen(value)`, not by `n`. (Its `n < 0` arm answers `false` where HotSpot throws — but nothing calls it) |
| `setBit` / `clearBit` / `flipBit` | **clear: not registered anywhere.** Worth recording that the JDK's own `setBit(n)` is `new int[Math.max(intLength(), intNum+2)]` — HotSpot allocates 256 MB for `ONE.setBit(Integer.MAX_VALUE)` itself, so a future implementation matching it is not a divergence, and `testBit` is genuinely the odd one out |
| `bitLength` / `bitCount` on negatives | **clear.** `bit_count`'s `to_twos(mag.len() + 1)` is sized by the OPERAND |
| `modPow` / `modInverse` / `is_probable_prime` | **clear.** Exponent-driven, but the exponent is an operand whose magnitude bounds the work |
| `bi_to_byte_array_str`, `bi_from_byte_array_*` | **clear** (sized by the operand) — and verified for correctness, §5 |
| `vec![0; n]` with `n` from an argument | one hit, `to_twos` in `test_bit` (§1). The five others in `bigint.rs` are sized by operand limb counts |
| `native_bd_divide_scale`, POSITIVE scale | **OPEN, see §6** |

---

## 5. Verifying E38-1's deletions rather than assuming them

E38-1 deleted registrations "with nothing written to replace them", on the
premise that a correct twin already existed. Checked, not taken on trust:

**Eight, not eleven, are in `math_bignum.rs`.** `git show 892b2ccb5` confirms the
deleted triples are `bitLength ()I`, `bitCount ()I`, `testBit (I)Z`, `and`, `or`,
`xor`, `not ()Ljava/math/BigInteger;`, `toByteArray ()[B`. The other three of the
eleven are the `StringBuilder.repeat` duplicates in `lang_string.rs`.

**Every one of the eight has a surviving body with the identical descriptor** in
`phases_late::register_p71_biginteger_extras` (lines 7968, 7978, 7988, 7998,
8002, 8016, 8020, 8024 at `892b2ccb5`), each computing on `BigInt` limbs. **No
triple is unregistered in either mode** — see §7 for the order trace that
establishes "either mode".

**`bi_to_byte_array_str` — the converter E38-1 named as the reason the deletion
was safe — is correct.** `ByteArr.java` transliterates it and diffs against
`BigInteger.toByteArray()`:

```
TOBYTEARRAY cases=13233 diffs=0
FROMBYTES   cases=13233 diffs=0
(-1).toByteArray() = [-1]      bi_to_byte_array_str("-1") = [255]
```

1,201 consecutive small integers, the byte/word/sign boundaries with both signs,
and 3,000 random values up to 400 bits each also taken negated and shifted left
to plant trailing zero bytes. The row E38-1 cited as the defect in the deleted
body — `(-1)` giving `{0xFF, 0xFF}` where HotSpot gives `{0xFF}` — is correct in
the survivor.

**The dependency E38-1 flagged is closed, by another lane.** It kept
`shiftLeft`/`shiftRight` registered "because deleting them would hand
synthetic-jdk mode the 256 MB allocation that still lives in `phases_late`". Lane
F2 has since landed that guard (`p71_bi_checked_shl`). See §7.

**And the same converter is still the reason NOMINATION 2 is open**:
`bi_from_byte_array_signed(&[])` answers `"0"` where
`new BigInteger(new byte[0])` is `NumberFormatException: Zero length BigInteger`
(MEASURED). That is `phases_late`'s call site, not this file's — E38-1's
NOMINATION 2, seconded below.

---

## 6. RESIDUALS — measured, deliberate, and not fixed

**`native_bd_divide_scale`'s POSITIVE half is still unbounded.** E38-1 fixed the
sign-losing `new_scale as usize` cast and left
`format!("{:.prec$}", a / b, prec = new_scale.max(0) as usize)`, so
`x.divide(y, Integer.MAX_VALUE, HALF_UP)` still asks `format!` for 2.1 billion
fractional digits. That is the mirror image of `[a fix that only pins the
positive half hides what it unmasked]`: here it was the NEGATIVE half that got
pinned.

It is left alone because the boundary is **not measurable in reasonable time**
and a guessed guard would be a divergence in one direction or the other. The two
extremes are measured:

```
1.5.divide(2, MAX_VALUE, HALF_UP) !! ArithmeticException: BigInteger would overflow supported range  [0 ms]
1.5.divide(2, MIN_VALUE, HALF_UP) !! ArithmeticException: Underflow                                  [0 ms]
```

but `1.5.divide(2, 715827883, HALF_UP)` — the scale at which the `TEN.pow`
predicate that governs `setScale` would refuse — did **not** return within 240 s
on HotSpot with `-Xmx4g`. So `divide(…, scale, …)` does not route through the
same predicate as `setScale`, and HotSpot is itself a denial of service in that
range. Matching an unrefused HotSpot is not a divergence; refusing where it does
not would be.

**`Character.digit` accepts non-ASCII digits** (E38-1's residual, deliberately
not re-broken): `new BigInteger("٣")` is 3 on HotSpot and
`NumberFormatException` here, because `char::to_digit` is ASCII-only. Nothing in
this lane widens or narrows that.

**`isProbablePrime` ignores `certainty` on both sides.**
`4.isProbablePrime(0)` and `4.isProbablePrime(-1)` are `true` on HotSpot
(`if (certainty <= 0) return true;`) — that arm is now honoured in this file's
body but not in `phases_late`'s (NOMINATION 3).

---

## 7. The cross-lane duplicate: which registrar actually runs later

Two lanes have now reasoned about this order and both stated it as "runs in
EVERY mode". Traced through `vm/src/vm/vm_init.rs` rather than assumed:

| build / mode | what runs | winner for `java/math/BigInteger` |
|---|---|---|
| `synthetic-jdk` feature, `use_synthetic_jdk == true` (vm_init.rs:1932-1934) | `register_builtins` = `register_essential_natives` **then** `register_synthetic_overrides` (lib.rs:21596-21601) | **`math_bignum::register_biginteger_natives`** — it runs SECOND |
| `synthetic-jdk` feature, real-JDK mode (vm_init.rs:2055) | `register_essential_natives_with_shims` only | `phases_late::register_p71_biginteger_extras` |
| default CLI build, no `synthetic-jdk` (vm_init.rs:2593) | `register_essential_natives_with_shims` only | `phases_late::register_p71_biginteger_extras` |

So `phases_late`'s registrar does **not** win in every mode: it wins in every
mode in which it is the only one. `math_bignum`'s runs in exactly one mode and
wins there.

Both copies of the shift guard were live, in different modes — there was no
unguarded path, but the two would drift. **Resolved by deleting this file's
copy**: `shiftLeft`/`shiftRight` are no longer registered in
`register_biginteger_natives`, and `bi_shift_arg` / `bi_checked_shl` /
`bi_mag_bits` are deleted with them. `phases_late::p71_bi_checked_shl` is now the
single implementation, reached in every mode, and it applies the identical
magnitude-bit rule (`p71_bi_mag_bits(v) + k > i32::MAX`). This is the
consolidation E38-1's own §1 asked for once F2 landed, and F2's NOMINATION 1
asked for from the other side.

`BigInt::magnitude_bits` is added and `pub(crate)` so the rule has one owner.
F2's measured constraint — the guard must be on **magnitude** bits, because
`(-2).shiftLeft(MAX-2)` is legal at `bitLength=2147483646`, one less than its
2,147,483,647 magnitude bits — is quoted at the definition, since that is the
distinction a future caller reaching for `bit_length()` will get wrong.

---

## NOMINATIONS

### 1. `native-builtins/src/phases_late.rs` — delete `p71_bi_mag_bits`, use `BigInt::magnitude_bits`

The magnitude-bit rule is now in two places instead of three.
`BigInt::magnitude_bits` (`bigint.rs`) is `pub(crate)` as of this commit and
carries F2's `(-2).shiftLeft(MAX-2)` measurement in its doc comment.

OLD (`phases_late.rs:8275-8282`):

```rust
fn p71_bi_mag_bits(v: &crate::bigint::BigInt) -> u64 {
    match v.mag_le().last() {
        Some(&top) if top != 0 => {
            (v.mag_le().len() as u64 - 1) * 32 + (32 - u64::from(top.leading_zeros()))
        }
        _ => 0,
    }
}
```

NEW: delete it, and in `p71_bi_checked_shl` (`phases_late.rs:8331`) replace

```rust
    if p71_bi_mag_bits(v) + u64::from(k) > i32::MAX as u64 {
```

with

```rust
    if v.magnitude_bits() + u64::from(k) > i32::MAX as u64 {
```

The doc comment on `p71_bi_mag_bits` should move to the call site or be dropped;
`BigInt::magnitude_bits` already carries the `bitLength()`-vs-magnitude warning
and F2's measurement verbatim.

### 2. `native-builtins/src/phases_late.rs` — the two byte-array constructors

**Seconded from E38-1, and re-measured.** `<init>([B)V` calls
`bi_from_byte_array_signed(&[])`, which answers `"0"`:

```
new BigInteger(new byte[0]) -> !! java.lang.NumberFormatException: Zero length BigInteger
  bi_from_byte_array_signed(&[]) = 0
```

`<init>(I[B)V` accepts any `signum` and does not reject signum 0 with a non-zero
magnitude. MEASURED contract: `new BigInteger(new byte[0])` →
`NumberFormatException("Zero length BigInteger")`; `new BigInteger(0, new byte[0])`
→ **0, legal**; `new BigInteger(2, new byte[]{1})` and
`new BigInteger(0, new byte[]{1})` → `NumberFormatException`;
`new BigInteger(-1, new byte[]{1})` → -1. JDK 25 `BigInteger.java:412-460` has
the messages (`"Zero length BigInteger"`, `"Invalid signum value"`,
`"signum-magnitude mismatch"`). The reader itself is correct on every non-empty
input (13,233 rows, 0 diffs) — only the two *validations* are missing, and they
belong at the constructors.

### 3. `native-builtins/src/phases_late.rs` — `isProbablePrime` is wrong on negatives and ignores `certainty`

MEASURED (`scratchpad/f7/Prime.java`, Microsoft OpenJDK 25.0.3+9):

```
(-7).isProbablePrime(10) = true      (-2).isProbablePrime(10) = true
(-4).isProbablePrime(10) = false     (-1).isProbablePrime(10) = false
4.isProbablePrime(0) = true          4.isProbablePrime(-1) = true      4.isProbablePrime(1) = false
```

JDK 25 `BigInteger.java:1156-1166` is `if (certainty <= 0) return true;` then
`BigInteger w = this.abs();`. `phases_late`'s body is

```rust
        let v = bi_read_int(ctx, obj_arg(args, 0)?);
        Ok(Some(Value::Int(if v.is_probable_prime() { 1 } else { 0 })))
```

and `BigInt::is_probable_prime` opens `if self.neg || self.is_zero() { return false; }`,
so `(-7).isProbablePrime(10)` is **false** where HotSpot says true, and the
`certainty <= 0` arm is not honoured at all.

NEW:

```rust
    r.register(bi, "isProbablePrime", "(I)Z", |ctx, args| {
        // JDK 25 BigInteger.java:1156 — `if (certainty <= 0) return true;`
        // then `BigInteger w = this.abs();`. MEASURED on 25.0.3+9:
        // (-7).isProbablePrime(10) = true, 4.isProbablePrime(0) = true.
        let certainty = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        if certainty <= 0 {
            return Ok(Some(Value::Int(1)));
        }
        let v = bi_read_int(ctx, obj_arg(args, 0)?);
        let w = if v.is_neg() { v.neg_value() } else { v };
        Ok(Some(Value::Int(if w.is_probable_prime() { 1 } else { 0 })))
    });
```

This file's copy — which wins in synthetic mode — has been fixed the same way in
this commit. Its previous body was worse in the direction that matters: trial
division capped at `i <= 10000` that **returned `true` when the cap was
reached**, so `(1000003*1000033).isProbablePrime(100)` and a 512-bit semiprime
were both reported prime where HotSpot reports composite. A key-generation path
trusts that answer.

### 4. `native-builtins/src/lib.rs` — `bigint_mul_pow10` builds an `n`-digit string

```rust
fn bigint_mul_pow10(bi: &crate::bigint::BigInt, n: i32) -> crate::bigint::BigInt {
    let mut p = String::with_capacity(1 + n as usize);
    p.push('1');
    for _ in 0..n { p.push('0'); }
    bi.mul(&crate::bigint::BigInt::from_decimal(&p))
}
```

`n` reaches it from `setScale`, `add`, `subtract` and `toBigInteger`, and
`from_decimal` on an `n`-digit string is O(n²) limb work on top of the O(n)
string. This lane's `bd_pow_ten_check` now guards the `setScale` call site, but
`math_bignum.rs:3311` and `:3329` (`add`/`subtract` scale alignment) reach it
with `s - sa` where both scales are caller-controlled, and `:3497`
(`toBigInteger`) with `-scale`. A `10^n` built by repeated squaring on limbs
would remove the O(n²), and the guard belongs in the helper rather than at each
of its four call sites.

### 5. `regression-suite/run.sh`

Thirded from `W8-E29-1` and E38-1. `RJdkBridge1` still is not registered, and it
is the only instrument that turns any of the above from PREDICTED into MEASURED.

---

## What `RJdkBridge1 --only=bigint` should do — PREDICTED, every row

E38-1 predicts this family goes from ~14 failing to 3 (the three being its
NOMINATION 2). This lane does not change that count:

| row | E38-1's prediction | after this lane | why |
|---|---|---|---|
| `ONE.testBit(-1)` → `ArithmeticException` | green | **green** | the `n < 0` check is in the registration, not in `test_bit`; untouched |
| the six `and`/`or`/`bitCount`/`bitLength` rows | green | **green** | untouched |
| `(-1).toByteArray()` | green | **green** | verified correct here (§5), not changed |
| `(-9).shiftRight(1) == -5`, `16.shiftLeft(-2)`, `16.shiftRight(-2)`, `ONE.shiftLeft(MIN)`, `ZERO.shiftLeft(MIN)` | green | **green** | the registration moved from `math_bignum` to `phases_late`; F2's body applies the same rule and its own probe measured these rows |
| the three `new BigInteger(byte[])` rows | still RED | **still RED** | NOMINATION 2, unchanged |
| the other 36 | green | green | |

**PREDICTED: `bigint` stays at 3 failing after this lane, all three
NOMINATION 2.** Nothing in this record is expected to flip a `RJdkBridge1` row,
because **the fixture does not exercise any of it**: there is no `testBit` with a
large address, no `pow` row at all, no `setScale` row at all, and no
`isProbablePrime` row. That is the honest headline — a denial-of-service shape
and a "composite reported prime" both score green on a fixture built to compare
answers on small operands. If `bigint` is extended, the six rows that would
have caught this lane's four defects are:

```java
check("ONE.testBit(MAX)",        BigInteger.ONE.testBit(Integer.MAX_VALUE),        false);
check("(-1).testBit(MAX)",       BigInteger.valueOf(-1).testBit(Integer.MAX_VALUE), true);
checkThrows("TEN.pow(1e9)",      () -> BigInteger.TEN.pow(1_000_000_000), ArithmeticException.class);
checkThrows("1.5.setScale(MIN)", () -> new BigDecimal("1.5").setScale(Integer.MIN_VALUE, RoundingMode.HALF_UP), ArithmeticException.class);
check("0.setScale(MAX).scale()", BigDecimal.ZERO.setScale(Integer.MAX_VALUE).scale(), Integer.MAX_VALUE);
check("semiprime.isProbablePrime", new BigInteger("1000036000099").isProbablePrime(100), false);
```

The first four are each a hang or a multi-hundred-megabyte allocation on the
pre-fix VM, so they need the fixture's per-check timeout to be real before they
are added.

---

## Honest summary

Four live argument-driven allocations, three fixed, 387,340 executed comparisons
against HotSpot 25.0.3+9 behind them — and **not one line of CratonVM built or
run**, so every "after" is PREDICTED.

Two things are worth carrying out of this beyond the individual fixes.

**A correct answer is not a clean answer.** `test_bit` was right on all 371,547
probe rows and cost 256 MB per call. This defect class is invisible to every
instrument the project currently points at `BigInteger`, and it is reachable
from one line of ordinary bytecode in every mode. The instrument that finds it
is a *cost* comparison, not an answer comparison — `TestBit.java`'s
`old-skipped=6237` column is the whole measurement.

**"Runs in every mode" was stated by two lanes and is false for one of them.**
The registration order that decides which of two duplicate bodies answers is
five call sites away in `vm_init.rs`, and both lanes that reasoned about it
reasoned from the registrar's own doc comment. §7 traces it. The duplicate is
now gone, but the lesson is that `[dup nati]` needs the *call* order, not the
*file* order, and the call order is in a different crate.
