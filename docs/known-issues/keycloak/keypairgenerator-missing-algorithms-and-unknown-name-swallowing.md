# KeyPairGenerator: missing Ed448/RSASSA-PSS registrations, plus unrecognized algorithm names get swallowed into a generic "Unknown" error message

Status: open — two related but distinct CratonVM bugs found together: missing algorithm registrations, and a
diagnostic bug that hides the actual requested algorithm name

Date observed: 2026-07-10/11 (fresh-binary rerun from current dev, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Finding 1: missing `KeyPairGenerator` registrations for `Ed448` and `RSASSA-PSS`

Two distinct algorithms fail to resolve via `KeyPairGenerator.getInstance(...)`:

- **`Ed448`** (`crypto/elytron :: ElytronCryptoSdJwtKeyBindingTest::testEdDSAKeyBindingWithEd448`, and the same
  test in `crypto/fips1402`):
  ```
  => java.lang.RuntimeException: java.security.NoSuchAlgorithmException: Unknown KeyPairGenerator not available
     org.keycloak.common.util.KeyUtils.generateEddsaKeyPair(KeyUtils.java:82)
  ```
  (the algorithm name in the message is wrong — see Finding 2 below; the actual requested algorithm, per the
  call site, is `"Ed448"`)

- **`RSASSA-PSS`** (`crypto/elytron :: ElytronSignatureAlgTest::signatureDefaultAlg`):
  ```
  => java.security.NoSuchAlgorithmException: Unknown KeyPairGenerator not available
     org.keycloak.crypto.elytron.test.ElytronSignatureAlgTest.signatureDefaultAlg(ElytronSignatureAlgTest.java:28)
  ```
  (test source: `KeyPairGenerator.getInstance("RSASSA-PSS").genKeyPair()` — again reported as "Unknown", see
  Finding 2)

By contrast, the sibling test `testEdDSAKeyBindingWithEd25519` (same class, same `generateEddsaKeyPair()` call
site, just a different curve name argument) fails with a *correctly-named* error:
`NoSuchAlgorithmException: Ed25519 KeyPairGenerator not available` — confirming `Ed25519` is *also* unregistered,
but its name is at least reported correctly (see Finding 2 for why `Ed448`/`RSASSA-PSS` aren't).

## Finding 2: unrecognized `KeyPairGenerator` algorithm names get replaced with the literal string "Unknown" in the exception message

`org.keycloak.common.util.KeyUtils.generateEddsaKeyPair(String curveName)`:

```java
public static KeyPair generateEddsaKeyPair(String curveName) {
    try {
        KeyPairGenerator keyGen = KeyPairGenerator.getInstance(curveName);
        return keyGen.generateKeyPair();
    } catch (Exception e) {
        throw new RuntimeException(e);
    }
}
```

`curveName` is passed straight through to `KeyPairGenerator.getInstance(...)` — for `Ed448` this should produce
`NoSuchAlgorithmException: Ed448 KeyPairGenerator not available` under standard JCA exception-message convention
(as `Ed25519` correctly does), but instead produces `Unknown KeyPairGenerator not available`. Same for
`RSASSA-PSS` at a completely different call site (`ElytronSignatureAlgTest`, not going through `KeyUtils` at
all) — meaning this isn't specific to one call site or method, it's in CratonVM's shared
`KeyPairGenerator.getInstance()` algorithm-resolution/not-found path itself.

This strongly suggests CratonVM's `KeyPairGenerator` registry recognizes a fixed set of algorithm names by
explicit pattern/string match (`RSA`, `EC`, `Ed25519`, `DSA`, etc.), and any name that doesn't match one of those
known patterns falls through to a generic "unknown algorithm" branch that constructs its
`NoSuchAlgorithmException` with a hardcoded `"Unknown"` placeholder instead of interpolating the actual
`algorithm` argument that was passed to `getInstance()`. `Ed25519` apparently *is* one of the explicitly-matched
names (hence its correctly-named exception), even though no working implementation is registered behind it.

This is a pure diagnostics/message-correctness bug, but a real one — it actively hides which algorithm was
actually requested from anyone debugging a `NoSuchAlgorithmException`, for every algorithm CratonVM doesn't
explicitly recognize by name.

## Next steps

1. Find CratonVM's `KeyPairGenerator.getInstance()` implementation (likely `native-builtins/src/jca/` per this
   session's earlier finding of `native-builtins/src/jca/key_factory.rs` — check for a sibling
   `key_pair_generator.rs` or similar) and fix the not-found branch to interpolate the actual requested algorithm
   name into the exception message instead of a hardcoded `"Unknown"`.
2. Separately, consider registering `Ed448` and `RSASSA-PSS` `KeyPairGenerator` support (or confirm they're
   intentionally out of scope) — `RSASSA-PSS` in particular is a standard, commonly-needed JCA algorithm name
   (RSA-PSS signatures per RFC 8017), not an exotic one.
3. Once the message-formatting bug is fixed, re-run the affected classes — the underlying "not available" gaps
   for `Ed448`/`Ed25519`/`RSASSA-PSS` will still need real implementations, but at least error messages will
   correctly identify what's missing for future triage.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-keypairgen-unknown -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.ElytronSignatureAlgTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-20260710.exe -JdkHome $jdk
```

Minimal standalone repro (no Keycloak needed): `KeyPairGenerator.getInstance("Ed448")` (or any
CratonVM-unrecognized algorithm name) under CratonVM — the resulting `NoSuchAlgorithmException` message will say
`"Unknown KeyPairGenerator not available"` instead of naming the actual requested algorithm.

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-v2-20260710-shard1\others-jit\logs\crypto_elytron.org.keycloak.crypto.elytron.test.{ElytronSignatureAlgTest,sdjwt.ElytronCryptoSdJwtKeyBindingTest}.out.log`,
2026-07-10/11 rerun with a binary built from current `dev`. Source:
`apps/keycloak/common/src/main/java/org/keycloak/common/util/KeyUtils.java` (line 77-84),
`apps/keycloak/crypto/elytron/src/test/java/org/keycloak/crypto/elytron/test/ElytronSignatureAlgTest.java` (line 28).
