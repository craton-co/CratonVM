# SunEC no-JIT perf: `Math.unsignedMultiplyHigh` intrinsic

**Status:** PARTIAL FIX landed on branch `fix/ec-nojit-jit-crypto`. Real SunEC EC under
`CRATONVM_REAL_JCA=1 CRATONVM_DISABLE_JIT=1` is now meaningfully faster (~22 % on an unloaded
box) and remains byte-identical to HotSpot. Full "sub-second" is *not* reached; see "What's left".

## Root-cause profile
Stack sampling (`--stack-dump-on-timeout=N`) of `ecprobe_tmp/EcSign2` (keygen + 2 signs) under the
interpreter shows ~all wall-clock is the **one-time** generator-table precompute
`sun/security/ec/ECOperations$Secp256R1GeneratorMontgomeryMultiplier.<clinit>` →
`DefaultMultiplier.pointMultiply` → field arithmetic in
`sun/security/util/math/intpoly/MontgomeryIntegerPolynomialP256.mult([J[J[J)V` (a ~2.4 kB fully
unrolled P-256 Montgomery multiply). Its single hottest **leaf** was
`java/lang/Math.unsignedMultiplyHigh(JJ)J`, called once per limb pair (hundreds of times per
`mult`, thousands of `mult`s to build the comb table).

CratonVM already intrinsifies `Math.multiplyHigh` (`native_math_multiply_high`) but **not** the
unsigned sibling `unsignedMultiplyHigh` (JDK 18+), so it ran as interpreted Hacker's-Delight
bytecode (~20+ ops + a method call) on the hottest leaf. HotSpot intrinsifies it (single `mulx`).

## Fix
`native-builtins/src/lang_math.rs` — register and implement `unsignedMultiplyHigh(JJ)J` for both
`java/lang/Math` and `java/lang/StrictMath`, exactly mirroring the existing `multiplyHigh`:

```rust
let result = ((a as u128) * (b as u128)) >> 64;   // a,b read as u64
Ok(Some(Value::Long(result as u64 as i64)))
```

Byte-identical to the JDK by construction (both compute `(a·b mod 2^128) >> 64` over unsigned
a, b). Unit test `vm/src/vm.rs::math_unsigned_multiply_high` pins the cases where unsigned ≠ signed
against HotSpot JDK-25 reference values.

## Verification
- `ecprobe_tmp/UmhVerify` (compiled with JDK 25) prints `unsignedMultiplyHigh`/`multiplyHigh` for
  8 edge cases (incl. `u64::MAX·u64::MAX`, `2^63·2^63`, negative operands). **Every value matches
  HotSpot byte-for-byte**, including the cases where unsigned and signed high products differ
  (e.g. `umh(ffff…,ffff…)=fffffffffffffffe`, `umh(deadbeef…,012345…)=00fd5bdeeeb2a01d`).
- Same probe does an EC **sign → verify** roundtrip: `true`, and a tampered-signature verify:
  `false` — proving the P-256 field arithmetic stays correct end-to-end through the new intrinsic.
- `EcSign2` keygen + 2 signs still produces valid signatures (64-byte P1363, 71/72-byte DER).
- Timing A/B (same box, interleaved to cancel parallel-agent load): intrinsic wins every round
  (unloaded ~60 s → ~47 s; under heavy contention 123 s→96 s and 231 s→109 s).

## What's left (out of scope here)
A JIT-allow-crypto experiment (gate `sun/security/util/math/intpoly/` + `sun/security/ec/` past the
`CRATONVM_DISABLE_JIT` kill-switch) was tried and **reverted** — it gave *no* measurable speedup,
even with `CRATONVM_JIT_ALLOW_PACKAGES` also lifting the per-package skip list. The giant unrolled
`mult` compiles, but every `unsignedMultiplyHigh` leaf still dispatches **out-of-line** through
`jit_invoke_dispatch`, so JIT buys nothing without a **JIT inline-intrinsic** that emits `mulx`
inline (none exists today for `multiplyHigh` either). The remaining levers, both larger surface:
1. JIT inline-intrinsics for `Math.(unsigned)multiplyHigh` (+ ensure the intpoly methods compile),
   so the unrolled Montgomery multiply runs as native code with the multiply-high inlined.
2. A native `MontgomeryIntegerPolynomialP256.mult`/`reduce` intrinsic (well-defined `long[]` in/out,
   but must replicate the exact limb layout byte-for-byte) — or replacing the SunEC SPI wholesale.
