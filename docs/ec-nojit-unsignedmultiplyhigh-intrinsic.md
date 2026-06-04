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

## Follow-up 1 — JIT inline-intrinsic for `Math.(unsigned)multiplyHigh` (LANDED)
The signed/unsigned high-multiply is now also a **JIT call-site intrinsic**: an
`invokestatic java/lang/Math.multiplyHigh(JJ)J` / `unsignedMultiplyHigh(JJ)J` in JIT-compiled
code emits a one-operand `IMUL r64` / `MUL r64` (RDX:RAX = RAX·r, high half in RDX) inline — no
out-of-line dispatch — mirroring HotSpot. Implemented in `jit/src/lib.rs`
(`JitIntrinsic::Math{,Unsigned}MultiplyHigh` + matcher) and `jit/src/x64.rs` (codegen ladder, next
to the `Math.min/max` long path). Verified byte-identical to HotSpot under JIT via a hot-loop probe
(`ecprobe_tmp/UmhMin`: signed `S` and unsigned `U` both match). This benefits **any** JIT-on
multiply-high user; it is a general optimization, independent of EC.

## JIT is a DEAD END for the EC suites — do not re-attempt
The whole point was to make EC fast under `--nojit` by JIT-compiling the SunEC field arithmetic.
That path is **conclusively dead**:
- A JIT-allow-crypto gate (let `sun/security/util/math/intpoly/`+`sun/security/ec/` compile past the
  `CRATONVM_DISABLE_JIT` kill-switch) was implemented and **reverted** — *zero* measurable speedup,
  even combined with the inline-intrinsic above and `CRATONVM_JIT_ALLOW_PACKAGES`.
- The decisive test: running `EcSign2` under **full JIT** (no kill-switch at all) takes **~92 s** —
  no faster than the interpreter+native-intrinsic (~46–65 s). So the JIT does **not** beneficially
  compile the fully-unrolled intpoly methods (`MontgomeryIntegerPolynomialP256.mult` is ~2.4 kB of
  bytecode); they either bail `jit_scan` or run no faster. Inlining the leaf cannot help when the
  enclosing method never becomes fast JIT code.

## What's left — the ONLY remaining lever (larger surface, unattempted)
A native `MontgomeryIntegerPolynomialP256.mult`/`reduce`/`square` intrinsic backed by a Rust P-256
field implementation. Well-defined `long[]` in/out, but it **must replicate the JDK's exact
Montgomery limb layout byte-for-byte** (radix, limb count, reduction constant) — a mismatch yields
silently-wrong crypto. Verify by cross-checking a CratonVM-produced signature against **HotSpot**
verify (self-consistent sign→verify on CratonVM is NOT sufficient). High risk; not done here.
