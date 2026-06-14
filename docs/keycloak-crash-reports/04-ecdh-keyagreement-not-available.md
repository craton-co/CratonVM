# 04 — `KeyAgreement`/`Cipher` "Algorithm ECDH not available"

**Status:** open (crypto-provider routing gap)
**Affected:** BCEcdhEsAlgorithmProviderTest (`encodeDecode`)
**Symptom:** `java.security.NoSuchAlgorithmException: Algorithm ECDH not available`
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
