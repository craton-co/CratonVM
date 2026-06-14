# 17 — P-384 / P-521 EC: progressive heap corruption of SunEC field elements

**Status:** OPEN — deep GC / memory-safety (or SunEC limb-arithmetic) bug. NOT a quick fix.
**Affected:** DefaultCryptoJWKTest (`publicEs256P384`, `publicEs256P521`),
BCECDSACryptoProviderTest (secp384r1/secp521r1 params),
DefaultCryptoSdJwtVPVerificationTest (`AltCnfCurves`).
**Symptom:** `IllegalArgumentException: native EC scalar multiply failed (bad point/scalar)`
from `sun.security.ec.ECOperations.multiply` (in keygen `calculatePublicKey` and ECDSA
`verifySignedDigest`).

## What it is NOT
- Not a missing curve: `sunec_point.rs` implements P-256/384/521 via RustCrypto
  `p256`/`p384`/`p521` crates.
- Not the OID/cert-verify bug (#14, fixed).
- Not value-specific scalars: a **single** P-384/P-521 keygen, sign, verify, cert-build,
  cert-verify all PASS in isolation (apps/probe/kccert/{EcP384,CertP384,EcP384Ops}.java).

## What it IS (isolated via apps/probe/kccert/EcLoop.java + CRATONVM_DBG_EC)
Under a **repeated** mix of keygen + sign + verify, P-384 keygen fails ~35/40 and P-521
keygen fails 40/40. Gated debug in `scalar_mul_*` shows the failure is **"point NOT ON
CURVE"** with a **corrupted generator point**:

```
P-384 Gy (correct): …b5f0 b8c00a60b1ce1d7e819d7a4 31d7c90ea0e5f
P-384 Gy (CratonVM):…b5f0 c0000000000000000000004 31d7c90ea0e5f   ← middle limbs zeroed
```
The corruption **accumulates** (later iterations zero even more of both X and Y). Since the
generator is a fixed, cached point, its in-heap coordinate data (SunEC
`ImmutableIntegerModuloP` montgomery-limb arrays, read back via `asBigInteger()`) is being
progressively corrupted by repeated EC operations.

## Likely locus
A GC / memory-safety issue exercised by the heavy allocation + re-entrant-invoke pattern of
`sunec_point.rs::native_ec_multiply` (each call does several `invoke`s and BigInteger/array
allocations; P-384/P-521 field elements are larger than P-256, so more allocation churn),
**or** a SunEC long/limb-arithmetic interpreter bug in the P-384/P-521 `asBigInteger`/
montgomery conversion that writes outside the limb array. The zeroed-tail pattern resembles
the young-gen/TLAB corruption family (cf. memory `reference_bug_d_cidr_jit_gc`). P-256 is
unaffected (smaller field, possibly different limb layout / less churn).

## Next steps (for a future pass)
- Run `EcLoop` under the moving-GC / no-GC toggles and the stray-stack debug
  (`CRATONVM_DBG_STRAYSTACK`) to confirm GC vs arithmetic.
- Audit `native_ec_multiply` pinning of the intermediate `ImmutableIntegerModuloP` / BigInteger
  refs across the `asBigInteger`/`toByteArray` re-entrant calls.
- Compare P-256 vs P-384 `IntegerPolynomialP*` limb read-back for an out-of-bounds write.
