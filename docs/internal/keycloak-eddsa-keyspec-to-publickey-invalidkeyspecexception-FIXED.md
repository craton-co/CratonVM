# EdDSA KeyFactory KeySpec reconstruction fixed

Status: fixed  
Observed: 2026-07-11  
Fixed: 2026-07-12

## Root cause

`KeyPairGenerator.getInstance("Ed25519"/"Ed448")` already routed to the real
curve-specific SunEC key-pair generator, but the synthetic
`KeyFactory.generatePublic(KeySpec)` implementation still fell through to its
fail-closed `InvalidKeySpecException` branch for both EdDSA algorithms.
Keycloak's `EdECUtilsImpl.createOKPPublicKey` imports OKP JWKs by passing an
`EdECPublicKeySpec` to this method, so both SD-JWT curve tests failed before a
signature could be verified.

## Resolution

`KeyFactory.generatePublic` now dispatches Ed25519 and Ed448 to their matching
JDK 25 SPI implementations:

- `sun.security.ec.ed.EdDSAKeyFactory$Ed25519`
- `sun.security.ec.ed.EdDSAKeyFactory$Ed448`

The standard `EdECPublicKeySpec` is pinned while the SPI is allocated and then
passed to `engineGeneratePublic`, yielding a concrete usable public key.

## Validation

On the Azure Linux host with JDK 25, the standalone repro generated a real key
pair for each curve, reconstructed its public key through
`KeyFactory.generatePublic(new EdECPublicKeySpec(...))`, and verified a fresh
signature with that reconstructed key for both Ed25519 and Ed448. The targeted
native-builtins test also covers the curve-to-SPI routing.

Using the same uniquely named CratonVM binary, the inherited
`testEdDSAKeyBindingWithEd25519` and `testEdDSAKeyBindingWithEd448` methods
passed independently in both Keycloak subclasses:

- `crypto/fips1402 :: FIPS1402SdJwtKeyBindingTest`
- `crypto/elytron :: ElytronCryptoSdJwtKeyBindingTest`

The FIPS class's broader seven-test run also exposed one separate, time-bound
RSA-4096 expiration failure after 148 seconds; both isolated EdDSA methods
passed, so that unrelated result does not affect this closure.
