# 17 — P-384 / P-521 EC: progressive heap corruption (JIT miscompile)

**Status:** FIXED — two parts: (1) JIT ban on `sun/security/util/math/intpoly/`
(`vm/src/jit/skip_list.rs`, commit `ca34440d`) cleared the "scalar multiply failed"
corruption; (2) a SECOND, later JWKT failure — `X509Certificate.verify(key)` NPE'ing at
`SignatureUtil.initVerifyWithParam` ("Cannot invoke initVerify on null") — is now also
fixed. `DefaultCryptoJWKTest` = **OK (10/10)**.

## Second cause + fix (`SignatureUtil` / `SharedSecrets`)
`X509CertImpl.verify` calls `SignatureUtil.initVerifyWithParam(sig, key, params)`, which
indirects through `SharedSecrets.getJavaSecuritySignatureAccess()`. That accessor is set
by `java.security.Signature.<clinit>` — but CratonVM no-ops that clinit (it also triggers
`Debug.getInstance`→Security-file read, same as Cipher), so the accessor stays **null** →
NPE for EVERY real EC/RSA `cert.verify(key)`. Fix: intercept
`sun.security.util.SignatureUtil.{initVerify,initSign}WithParam` (`jca/signature.rs`) to
drive our registered `Signature.{initVerify,initSign,setParameter}` natives directly,
bypassing the null accessor. (Original `intpoly` note below.)

**Affected:** DefaultCryptoJWKTest (`publicEs256P384`/`P521`, now ✓);
BCECDSACryptoProviderTest (secp384/521 — separate `ECPublicKeySpec` issue remains);
DefaultCryptoSdJwtVPVerificationTest (`AltCnfCurves`, now ✓).
**Symptom:** `IllegalArgumentException: native EC scalar multiply failed (bad point/scalar)`
from `sun.security.ec.ECOperations.multiply` (keygen `calculatePublicKey`, ECDSA verify).

## Root cause — a JIT miscompile
A single P-384/P-521 keygen/sign/verify passes; under a **repeated** keygen+sign+verify mix
the curve's field-element limb arrays (`long[]`) are progressively corrupted — chunks of the
cached generator point's coordinates get zeroed, so `ECOperations.multiply` then reports
**"point NOT ON CURVE"** (gated `CRATONVM_DBG_EC` debug confirmed):
```
P-384 Gy correct : …b5f0 b8c00a60b1ce1d7e819d7a4 31d7c90ea0e5f
P-384 Gy CratonVM: …b5f0 c0000000000000000000004 31d7c90ea0e5f   ← limbs zeroed
```

`CRATONVM_DISABLE_JIT=1` → **0/40 fail** ⇒ it is a JIT miscompile. Package bisection
(`CRATONVM_JIT_BISECT_ONLY`, no rebuild) showed each package alone is clean but
**`java/math` + `sun/security/util/math/intpoly` JIT-compiled together** reproduces 35/40.
This is the JIT-only face of the documented cross-package JIT→JIT arg-marshalling miscompile
(a primitive value lands in a reference/array slot — see
`docs/bc-math-ec-jit-miscompile-investigation.md`): `intpoly` is the compiled *caller*, and a
JIT→JIT call into compiled `BigInteger` mis-marshals an operand slot, writing a primitive
into a limb. P-256 is unaffected (smaller field / fewer limbs).

## Fix
Ban `sun/security/util/math/intpoly/` from the JIT (interpret it) — the established codebase
pattern for correctness-critical, non-benchmarked code with a JIT miscompile (cf. the
`org/bouncycastle/` and `net/bytebuddy/` bans). With `intpoly` interpreted, the bad JIT→JIT
call never forms. EC field math is never a benchmarked hot path, so interpreter-only is the
right trade; overridable via `CRATONVM_JIT_ALLOW_PACKAGES=sun/security/util/math/intpoly/`.

Verified: P-384/P-521 keygen+sign+verify 0/40 fail; DefaultCryptoJWKTest 10/10.

## Note — the underlying general JIT bug remains
The real defect is the cross-package JIT→JIT operand-slot/arg-marshalling miscompile in
`jit/src/x64.rs` (suspected category-2/long arg slot accounting; see the BC-EC investigation
doc). The ban suppresses its EC face; a proper codegen fix would also clear the
`org/bouncycastle/math/ec` ban and other latent cases — a separate, larger JIT effort.
