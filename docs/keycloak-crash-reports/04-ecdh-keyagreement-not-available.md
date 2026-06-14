# 04 — `KeyAgreement`/`Cipher` "Algorithm ECDH not available"

**Status:** ✅ FIXED — `BCEcdhEsAlgorithmProviderTest` now `OK (2 tests)` (`deriveKey`
+ full `encodeDecode` ECDH-ES JWE round trip); 0 regressions.

## Fix (two parts)
1. **`KeyAgreement` was un-intercepted** → real `KeyAgreement.getInstance("ECDH")`
   hit the empty provider list → `NoSuchAlgorithmException`. Added a
   `javax.crypto.KeyAgreement` native module (`jca/key_agreement.rs`) that drives the
   real `sun.security.ec.ECDHKeyAgreement` SPI (getInstance/init/doPhase/
   generateSecret), stashing the SPI in a GC-scanned synthetic slot. ConcatKDF /
   AES-KeyWrap stay real BouncyCastle bytecode.
2. **`ECDHKeyAgreement.validate` order check failed.** SunEC validates the peer key by
   computing `n·P` and asserting `isNeutral` (the point-at-infinity). CratonVM's native
   EC scalar-multiply intrinsic (`sunec_point.rs`) returned `None` for an identity
   result — treating the *expected* outcome as "bad point/scalar". Fixed: the macro now
   signals identity (empty coords) and `native_ec_multiply` returns SunEC's neutral
   `ProjectivePoint$Mutable` (fresh `Z=0`); a non-canonical scalar (`>= n`, only seen in
   this order check over an already-on-curve point) is reduced to identity. Keygen/sign
   scalars (`< n`) are unaffected.

(historical) **Symptom:** `java.security.NoSuchAlgorithmException: Algorithm ECDH not available`
(wrapped in `JWEException`) during ECDH-ES key agreement.

## Analysis
The ECDH-ES JWE algorithm needs `KeyAgreement.getInstance("ECDH", BC)`. CratonVM's
synthetic JCA layer doesn't expose ECDH key agreement, and (unlike EC signatures, which
are routed to real SunEC — see memory `route_ec_to_real`) ECDH is not routed to the real
BouncyCastle/SunEC provider, so the lookup fails closed. Parallels the ML-DSA/ML-KEM
"route to real BC like EC" pattern already used elsewhere.

## Fix direction
Route `KeyAgreement` "ECDH"/"ECDHwith…" to the real provider the same way EC signature
algorithms are routed, instead of failing in the synthetic layer.
