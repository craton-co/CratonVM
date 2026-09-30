# JIT round 14, lane bigdec: proposals (ranked)

Status: OPEN (proposal book; ideas, not work items)
Area: `native-builtins/src/math_bignum.rs`, `native-builtins/src/bigint.rs`, `native-builtins/src/phases_late.rs` (BigInteger Bridges)
Severity: proposals
Found by: round 14 wave 2 lane bigdec

Context: wave 2 landed BD4-2 (`bigint_mul_pow10` through the `5^n` memo) and BD4-1 (chunked +
divide-and-conquer `BigInt::from_decimal`, `CRATONVM_BIGINT_FROM_DECIMAL_FAST`), plus limb roads
for the legacy `mag_words_to_decimal` (`CRATONVM_BIGINT_MAG_TO_DECIMAL_CHUNKED`) and the
decimal-string `gcd` / `toByteArray` helpers (`CRATONVM_BIGINT_STR_HELPERS_LIMB`). Reading the
registrars showed that in the default real-JDK mode none of the Java-level string constructors
reach a native.

## BD5-1. Real-JDK `BigInteger.<init>(String)` / `(String, int)` natives

**What.** Only the synthetic-jdk registrar (`register_biginteger_natives`, `lib.rs`
`register_synthetic_overrides`, `#[cfg(feature = "synthetic-jdk")]`) registers
`native_bi_init_string{,_radix}`; real-JDK mode runs `BigInteger.java:526-602` (a
`substring` + `Integer.parseInt` + `destructiveMulAdd` per 9 digits). `bi_parse_java` already
transcribes the JDK's validation order and messages (`Radix out of range`, `Zero length
BigInteger`, `Illegal embedded sign character`, the `For input string: "<group>"[ under radix r]`
group). A real-mode `Intrinsic` registration (in `register_biginteger_arithmetic_overrides`) would
parse with `BigInt::from_decimal` (radix 10) and write `signum`/`mag` directly
(`bi_alloc_int`-style, no decimal string). **Preconditions** (each a wrong answer if skipped):
`Character.digit` semantics (Unicode `Nd` digits and full-width Latin letters are digits to the
JDK; `bi_java_digit` is ASCII-only -- see
`r14w2-bigdec-synthetic-biginteger-ctor-rejects-unicode-digits-FIXED-20260929.md`), `reportOverflow`
("BigInteger would overflow supported range", `numBits + 31 >= 2^32`) and `checkRange` for
`MAX_MAG_LENGTH`, and `mag = ZERO.mag` for a zero value. **Benefit.** Every `new BigInteger(s)`
of a long string: one native call instead of an interpreted/compiled group loop with two
allocations per 9 digits. **Cost.** ~60 lines plus a differential of the probe's `nfe` rows.
**Risk.** Medium (message parity); `stub_ratchet` `BASELINE_INTRINSICS` +2. **First step.**
Run `R14BigdecParse` `bi-parse`/`nfe` on HotSpot vs CratonVM to size the gap, then the
`Character.digit` table.

## BD5-2. Real-JDK `BigDecimal.<init>(String)` native

**What.** Same shape for `BigDecimal(String)` -> `BigDecimal(char[], int, int, MathContext)`
(JDK 25 `BigDecimal.java` ~ 480-760): the compact (<= 18 digit) road and the inflated road
(`new BigInteger(coeff, sign, prec)`), with the JDK's NumberFormatException messages ("Character
x is neither a decimal digit number, decimal point, nor \"e\" notation exponential mark.",
"Exponent overflow.", "No digits found.", "Character array contains more than one decimal
point.", ...) and its `precision` field (set, not the lazy 0). `native_bd_init_string` exists for
synthetic mode but was never checked against those messages. **Benefit.** `new BigDecimal(s)` is
the dominant JDBC/JSON decimal road. **Cost.** ~150 lines + a message-by-message differential.
**Risk.** Medium-high (message text, `precision`). **First step.** Transcribe the parse into a
pure `fn bd_parse_jdk(&str) -> Result<(BigInt, i32, i32), String>` with unit rows from the probe's
`nfe` output, registering nothing.

## BD5-3. One per-thread power table for both decimal conversions

**What.** `to_decimal_dc` (BD4-4) and the new `parse_dc` each square `10^9` up to `10^(9*2^k)` on
every call. A per-thread memo of the levels (same bounded shape as `bigint_pow5_memo`) shared by
both removes ~40% of a wide `toString()` and ~30% of a wide parse. **Benefit.** Repeated
conversions of 5 000+-digit values. **Cost.** ~40 lines. **Risk.** Low (pure values). **First
step.** Move the level list into a `thread_local!` in `bigint.rs` keyed by `k`.

## BD5-4. Radix parse in `bi_parse_java` is one `BigInt` multiply + add + two allocations per char

**What.** The non-10 radix arm of `bi_parse_java` builds `acc = acc * radix + d` with fresh
`BigInt`s per character: O(n^2) with an allocation per digit. Chunk like `parse_chunked`
(`digitsPerInt[radix]` digits per pass, `intRadix[radix]` as the multiplier), and for radix
powers of two assemble the words directly. Synthetic-mode today; needed by BD5-1. **Cost.** ~30
lines. **Risk.** Low. **First step.** Differential against the per-char loop for radix 2..36.

## BD5-5. Retire the decimal-string `BigInteger` helpers behind `--compatible` Bridges

**What.** `phases_late::register_p71_biginteger_extras` still reads `gcd`, `toByteArray`,
`intValueExact`, `longValueExact` through `bi_read` (limbs -> decimal) and answers through
`bi_alloc` (decimal -> limbs). Wave 2 made both conversions and the two helpers limb-backed, but
the round trip is still paid. Exact patch:
`r14w2-bigdec-phases-late-biginteger-decimal-roundtrips-patch-FIXED-20260929.md`. **Benefit.** No
decimal text on those four natives. **Risk.** Low.
