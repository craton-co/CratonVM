# Continue: real SunEC EC is correct but slow under `--nojit` (108s one-time + ~16s/op)

> **RESOLVED — merged to `dev`** (merges `27e2b33` + `cdab8f7`). Per-op EC cost ~1–16s → **~135ms
> (sub-second)** via a byte-identical native `MontgomeryIntegerPolynomialP256.{mult,square}` intrinsic
> (`native-builtins/src/sunec_intpoly.rs`), plus native + JIT-inline `Math.(unsigned)multiplyHigh`.
> Cross-VM verified (CratonVM-sign ↔ HotSpot-verify both directions) + 9 byte-identical JDK limb
> vectors. `EcSign2` ~47–60s → ~15s; residual ~14s is the one-time generator table (amortized).
> JIT path is a proven dead end (don't re-try). Full writeup:
> `docs/ec-nojit-unsignedmultiplyhigh-intrinsic.md`. Original partial-fix note below (superseded).

> **PARTIAL FIX (branch `fix/ec-nojit-jit-crypto`, worktree `C:\craton\CratonVM-ecjit`):**
> Added the missing `Math/StrictMath.unsignedMultiplyHigh(JJ)J` native intrinsic
> (`native-builtins/src/lang_math.rs`) — the single hottest leaf in the P-256 Montgomery field
> multiply (`MontgomeryIntegerPolynomialP256.mult`). ~22 % faster (unloaded ~60 s → ~47 s for
> EcSign2 keygen+2sign), byte-identical to HotSpot (verified 8 edge cases + sign→verify roundtrip,
> `ecprobe_tmp/UmhVerify`). Mirrors HotSpot, which intrinsifies it too. NOT yet sub-second — the
> bulk is the one-time generator-table `<clinit>`, dominated by interpreted intpoly arithmetic.
> A JIT-allow-crypto experiment was tried and reverted (no measurable gain — leaf calls still
> dispatch out-of-line). Remaining levers (both larger surface) documented in
> `docs/ec-nojit-unsignedmultiplyhigh-intrinsic.md`: JIT inline-intrinsic for (unsigned)multiplyHigh,
> or a native `MontgomeryIntegerPolynomialP256.mult` intrinsic / SPI replacement.


**Severity:** medium (perf, not correctness). After the EC-key fix (`continue_prompt_keycloak_ec_key.md`,
RESOLVED), real JDK-25 SunEC EC runs correctly under CratonVM in real-JCA mode, but it's slow enough that
EC-heavy suites are impractical under the interpreter.

## Symptom / measurements (no-JIT interpreter, `CRATONVM_REAL_JCA=1 CRATONVM_DISABLE_JIT=1`)
- The **first** EC operation pays a one-time **~108 s** cost: `sun.security.ec.ECOperations$Secp256R1Generator
  MontgomeryMultiplier.<clinit>` precomputes the fixed-base generator table (many P-256 point ops).
- **Subsequent** EC ops are ~1–16 s each (still slow — each is multi-limb modular arithmetic in
  `sun.security.util.math.intpoly.IntegerPolynomialP256` interpreted bytecode).
- Net: the 8-class Keycloak SD-JWT sweep (~95 tests) takes tens of minutes; a single class ~2–4 min.
  Confirmed via stack-dump (`--stack-dump-on-timeout`) that the hang is genuine forward progress in
  `ECOperations.multiply` → `IntegerPolynomial.setProduct`, not a deadlock.

## Options (pick per direction)
1. **JIT the hot EC math.** With JIT on, the field arithmetic / point ops should compile and run fast. BUT
   JUnit-4 under JIT is currently blocked by a separate `java/lang/invoke/LambdaForm.<clinit>` bug (that's why
   the EC work used `CRATONVM_DISABLE_JIT=1`). Path A: fix/avoid the LambdaForm-under-JIT bug so the suites can
   run JIT-on. Path B: allow JIT selectively for the `sun.security.util.math.intpoly.*` / `sun.security.ec.*`
   hot methods even when the harness otherwise runs `--nojit`.
2. **Native EC intrinsic.** Intercept the hottest SunEC primitives (e.g. `ECOperations.multiply` /
   `IntegerPolynomialP256` mul/reduce, or the SunEC keygen/sign/verify SPI) with a `native-builtins`
   implementation backed by a real Rust EC backend (a `crypto_impl::Ecdsa`/P-256 already exists). This mirrors
   HotSpot, which itself *intrinsifies* these. Must return byte-identical results and keep the real
   `ECPublicKeyImpl`/`ECParameterSpec` object types (the EC-key fix depends on real types). Lowest risk to
   correctness if scoped to the field-arithmetic inner loop; bigger surface if you replace the whole SPI.

## Verify
A single EC keygen+sign+verify probe (`ecprobe_tmp/EcVerifyProbe`, `EcSign2`) should drop from ~110 s to
sub-second. Then the SD-JWT cluster should finish in a couple minutes. Re-check the EC-key tests stay green
(`DefaultCryptoSdJwsTest` = OK(14)) and outputs stay byte-identical to HotSpot.

## Notes
Build/run with the **renamed-binary** trick (parallel agent loops `taskkill //IM cargo.exe|rustc.exe|cratonvm.exe`)
— see `continue_prompt_sdjwt_verify_ordering.md`. Memory ref: `reference_jca_synthetic_crypto_layers`,
`reference_native_registration_paths`.
