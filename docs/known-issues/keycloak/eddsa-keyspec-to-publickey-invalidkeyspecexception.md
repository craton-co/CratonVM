# EdDSA (Ed25519/Ed448): reconstructing a `PublicKey` from a `KeySpec` fails with `InvalidKeySpecException`

Status: open — narrower successor to the previously-fixed "KeyPairGenerator not available" bug; key *pair
generation* now works, but reconstructing a public key from a spec (as done when parsing a JWK) still fails

Date observed: 2026-07-11 (refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

`crypto/elytron :: ElytronCryptoSdJwtKeyBindingTest` and the FIPS1402 sibling both fail on both EdDSA curve tests:

```
=> java.lang.RuntimeException: java.security.spec.InvalidKeySpecException: cannot generate a usable Ed448 public key from the given KeySpec
   org.keycloak.jose.jwk.EdECUtilsImpl.createOKPPublicKey(EdECUtilsImpl.java:110)
   org.keycloak.jose.jwk.JWKParser.toPublicKey(JWKParser.java:84)
   org.keycloak.util.JWKSUtils.getKeyWrapper(JWKSUtils.java:127)
```

(same signature for both `Ed448` and `Ed25519` — confirmed via `testEdDSAKeyBindingWithEd448` and
`testEdDSAKeyBindingWithEd25519`, both in the same class).

## Notes

- This is a **different, narrower** symptom than the previously-fixed
  `keypairgenerator-missing-algorithms-and-unknown-name-swallowing.md`/
  `keycloak-keypairgenerator-algorithms-and-name-diagnostics-FIXED.md` finding — that was about
  `KeyPairGenerator.getInstance("Ed25519"/"Ed448")` failing outright (`NoSuchAlgorithmException`). That's now
  fixed (key pair *generation* works), but a *different* code path — reconstructing a `PublicKey` object from a
  `KeySpec` (used when parsing a JWK's public key material back into a Java key object, e.g. for verifying a
  signature against a JWK received over the wire) — still fails for both EdDSA curves.
- `org.keycloak.jose.jwk.EdECUtilsImpl.createOKPPublicKey` is Keycloak's own utility wrapping the JDK's
  `KeyFactory.generatePublic(KeySpec)` for OKP (Octet Key Pair, the JWK key type for EdDSA) keys — the failure is
  inside whatever `KeyFactory` call this makes.

## Next steps

1. Read `org.keycloak.jose.jwk.EdECUtilsImpl.createOKPPublicKey` (line ~110) to see the exact `KeyFactory`/
   `KeySpec` construction used, then find CratonVM's corresponding `KeyFactory.generatePublic()` implementation
   for `Ed25519`/`Ed448`/`XDH`-family algorithms and check why it rejects the spec.
2. Write a minimal standalone repro: build an `EdECPublicKeySpec` (or whatever spec type Keycloak uses) from a
   known-good Ed25519/Ed448 public key's raw bytes, then call
   `KeyFactory.getInstance("Ed25519").generatePublic(spec)` directly under CratonVM to isolate from Keycloak/JWK
   parsing entirely.
3. Verify the fix against both `testEdDSAKeyBindingWithEd448`/`testEdDSAKeyBindingWithEd25519` in both
   `ElytronCryptoSdJwtKeyBindingTest` and `FIPS1402SdJwtKeyBindingTest`.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-eddsa-keyspec -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.sdjwt.ElytronCryptoSdJwtKeyBindingTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh-20260711.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh-shard1\all-jit\logs\crypto_elytron.org.keycloak.crypto.elytron.test.sdjwt.ElytronCryptoSdJwtKeyBindingTest.out.log`
and the `crypto_fips1402` sibling, 2026-07-11 refresh rerun with a binary built from current `dev`. Source:
`apps/keycloak/core/src/main/java/org/keycloak/jose/jwk/EdECUtilsImpl.java` (line 110).
