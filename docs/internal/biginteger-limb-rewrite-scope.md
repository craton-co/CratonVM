# Scope: limb-based `java.math.BigInteger` rewrite

Status: **proposed** (scoping only — no code yet). Author handoff, 2026-05-31.

## 1. Problem

CratonVM's `BigInteger` natives are correct but **catastrophically slow** for
crypto-sized operands. BouncyCastle `PrimesTest` does not finish in **10
minutes** (HotSpot: ~seconds); `bc-math-ec`, `bc-crypto-regression`,
`bc-pqc-crypto-regression` all time out at 150 s; RSA/EC/DSA key-gen and
Miller-Rabin are unusably slow. This blocks every crypto-heavy app suite.

The cause is the representation, not the algorithms.

## 2. Current architecture (the root cause)

A `BigInteger` object is stored in the **real-JDK layout** — `signum:I` +
`mag:[I` (big-endian, base 2³², the same words HotSpot uses). Confirmed in
`bi_alloc` / `bi_read` (`native-builtins/src/lib.rs:23058,23145`).

But **every native operation round-trips through a decimal string**:

```
mag:[I  --bi_read-->  "12345…" (decimal String, O(n²) convert)
        --bi_*_str-->  decimal-string arithmetic  (O(digits²) per op)
        --bi_alloc-->  mag:[I   (decimal→words, O(n²) convert)
```

So a single 256-bit `modPow` (hundreds of mul+mod) pays the O(n²)
words→decimal→words conversion **and** O(digits²) decimal schoolbook
arithmetic on every inner step. For a 77-digit number that is ~6000 digit-ops
per multiply where ~64 word-ops would do — an ~100× constant-factor loss,
compounded over thousands of modmuls.

### Decimal primitives to replace (all in `native-builtins/src/lib.rs`, ~38 fns)

`bi_add_unsigned` `bi_sub_unsigned` `bi_mul_unsigned` `bi_div_unsigned`
`bi_mod_unsigned` `bi_cmp_unsigned` `bi_add_str` `bi_sub_str` `bi_mul_str`
`bi_div_str` `bi_mod_str` `bi_compare` `bi_gcd_str` `bi_mod_pow_str`
`bi_mod_inverse_str` `bi_shift_left_str` `bi_shift_right_str` `bi_test_bit_str`
`bi_bit_length_str` `bi_bit_count_str` `bi_bitwise_and/or/xor` `bi_not_str`
`bi_to_byte_array_str` `bi_from_byte_array_*` `bi_to_binary` `bi_from_binary`
+ the `mag_words_to_decimal` / `decimal_to_mag_words` bridges.

Callers: **only** `lib.rs` + `phases_late.rs` (the two BigInteger native
registration sites — 39 `register` calls total). No external callers, so the
blast radius is contained to the BigInteger surface.

## 3. Goal

Operate directly on the base-2³² word representation that is **already stored**
in `mag:[I`. Eliminate the decimal round-trip from every arithmetic op.
Decimal is then needed ONLY at the String boundary (`toString(radix)` /
`new BigInteger(String)`), which is rare and non-hot.

Target: `PrimesTest` and the BC crypto/EC/pqc suites complete and pass within
their envelopes; no correctness or regression-pool change.

## 4. Design

### 4.1 Core type
Introduce an internal signed bignum, e.g.

```rust
struct BigInt { neg: bool, mag: Vec<u32> }  // mag little-endian, normalized (no trailing 0; empty = zero)
```

All arithmetic operates on `BigInt`. Keep it private to a new module
`native-builtins/src/bigint/` (mod.rs + ops) to isolate it.

### 4.2 New read/write boundary (the key change)
- `bi_read_int(ctx, this) -> BigInt`: read `signum` + `mag:[I` **directly into
  `Vec<u32>`** (no decimal). Reverse big-endian→little-endian. Handle the
  synthetic-String fallback by parsing decimal only in that mode.
- `bi_alloc_int(ctx, &BigInt) -> ObjectRef`: write `signum` + `mag:[I`
  **directly** (little→big-endian). No decimal.

`bi_read`/`bi_alloc` (decimal String) stay only as thin wrappers used by
`toString`/`<init>(String)`.

### 4.3 Operations to implement on `BigInt` (with algorithm + reference)
| Op | Algorithm | Notes |
|---|---|---|
| add/sub | schoolbook word add/sub w/ carry/borrow | sign handling |
| mul | schoolbook (Comba); Karatsuba optional later | products in u64 |
| divmod | **Knuth Algorithm D** (base 2³²) | the hard one; needs normalization + the 2-word quotient estimate + add-back. Validate hard. |
| mod | from divmod | |
| modPow | square-and-multiply on words; **Montgomery** for odd modulus (the crypto case) → big speedup | fall back to binary-divmod modmul for even modulus |
| modInverse | binary extended GCD on words | |
| gcd | binary GCD (Stein) on words | |
| shiftLeft/Right | word + bit shift | arithmetic shift for negatives (two's-complement semantics) |
| and/or/xor/not/andNot | **two's-complement** word ops | JDK semantics for negatives — get this exactly right (BC ASN.1 + masks depend on it) |
| testBit/setBit/flipBit/bitLength/bitCount/getLowestSetBit | word/bit | two's-complement for negatives |
| compareTo | sign then magnitude | |
| toByteArray / `<init>([B)` | two's-complement big-endian | crypto-critical (keys, signatures) |
| toString(radix) / parse | base conversion (decimal via 10⁹ chunks) | the only place decimal remains |

### 4.4 Small-value fast path (optional, recommended)
When `mag.len() <= 2` (fits i128), do native i128 arithmetic — covers the very
common small-BigInteger case (loop counters, small constants) with zero
allocation. Already partially attempted historically (the `parse::<i128>`
helpers); fold in cleanly.

## 5. Migration plan (incremental, each step independently shippable)

1. **Land `bigint` module + `BigInt` type + read/write boundary + add/sub/mul/
   compare/shift, unit-tested against the existing decimal impls** (keep both;
   route nothing yet). Pure addition, zero risk.
2. **Knuth divmod + mod**, exhaustively tested vs decimal `bi_div/bi_mod` over
   random magnitudes (incl. divisor edge cases, normalization boundaries).
3. **Route the hot ops** (`mul`, `mod`, `modPow`, `divmod`) through `BigInt`.
   Re-run BC crypto/EC/pqc + `PrimesTest`. This is where the speedup lands.
4. **two's-complement bit ops + toByteArray/fromByteArray** through `BigInt`.
   Re-run BC ASN.1 RegressionTest (exercises these heavily).
5. **gcd / modInverse / remaining ops**; retire the decimal `bi_*_str` helpers
   (keep only toString/parse).
6. **Montgomery modPow** for odd moduli (perf polish for RSA/EC).

After each step: `cargo test -p cratonvm-native-builtins` + regression pool 14/14
+ the relevant BC suite.

## 6. Validation strategy (crypto-correctness is the #1 risk)

- **Differential unit tests**: keep the decimal impls during the rewrite and
  assert `BigInt::op` == decimal `bi_*_str` over thousands of random operands
  per op (sizes 0, 1-word, 2-word, 8-word, 16-word; both signs; edge cases:
  0, ±1, powers of two, all-ones, divisor>dividend, exact division). This is
  the safety net the reverted fast-path attempt **lacked** (it accidentally
  compared limb-vs-limb after replacing the reference).
- **Known-answer vectors** vs HotSpot for modPow/modInverse/gcd/toByteArray.
- **BC suites as integration oracles**: `bc-asn1` (bit ops, byte arrays),
  `bc-crypto-prng`, `bc-math-raw` (long-bit canary), `bc-math-ec`, `PrimesTest`.
- **Regression pool 14/14** after every routing step.
- Pre-existing baseline to respect: `bc-crypto-prng` HMacDRBG #9.1 already fails
  on clean dev (NOT BigInteger-related; don't be alarmed if it persists).

## 7. Risks
- **Knuth divmod correctness** — most error-prone; the quotient-digit estimate +
  add-back is subtle. Mitigate with exhaustive differential tests.
- **two's-complement negatives** — and/or/xor/not/shiftRight/toByteArray for
  negative values must match the JDK bit-for-bit; BC relies on it.
- **Real-JDK vs synthetic layout** — must handle both `mag:[I` and the
  String-stub fallback in the read/write boundary.
- **JIT operand-stack long-bit interplay** is a SEPARATE concern
  (`docs/bc-ec-mod-mododdinverse-investigation.md`); this rewrite is native-side
  only and does not touch it.

## 8. Effort estimate
~1500–2000 LOC net in an isolated module; ~3–5 focused sessions following the
6-step plan (divmod + two's-complement bit ops are the bulk). Each step lands
green independently, so partial progress is always shippable.

## 9. Why not a crate (`num-bigint`)?
Not in the dependency tree (`Cargo.lock` has only `num-traits`), adding it needs
crates.io access and a new dependency the project has so far avoided for its
core numerics. A self-contained module keeps control of the `mag:[I` layout
interop and the small-value fast path. (Revisit if a dep becomes acceptable —
it would collapse steps 1–6 into "wire `num-bigint` at the read/write
boundary".)
