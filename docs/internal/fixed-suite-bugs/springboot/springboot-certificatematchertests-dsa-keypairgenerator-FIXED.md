# `CertificateMatcherTests` DSA `KeyPairGenerator` gap — FIXED

**Status: fixed 2026-07-17.**

## Original symptom

All four parameterized methods in `CertificateMatcherTests` aborted while the
`CertificateMatchingTestSource` method source built its fixtures. JUnit
reported `tests=0 failed=0 containersFailed=4`, because the failure occurred
before the test arguments existed:

```
java.security.NoSuchAlgorithmException: DSA KeyPairGenerator not available
    org.springframework.boot.autoconfigure.ssl.CertificateMatchingTestSource$Algorithm.generateKeyPair(CertificateMatchingTestSource.java:108)
```

## Root cause and fix

The real-JDK JCA shim already recognized `DSA`, and already routed DSA
`KeyFactory` and `Signature` operations to the JDK 25 SUN provider. Its
`KeyPairGenerator` dispatch, however, fell through after the RSA/EC cases and
raised `NoSuchAlgorithmException`.

`../../../../native-builtins/src/jca/key_factory.rs` now routes DSA to the real
`sun.security.provider.DSAKeyPairGenerator$Current` implementation, with the
same `initialize(int, SecureRandom)` and `generateKeyPair()` drive used for
the other real-provider key generators. It defaults DSA to 2048 bits, matching
the JDK generator's normal path, and returns concrete JDK DSA key objects
rather than a synthetic key shape. `DSS` remains the accepted alias.

## Validation

- `cargo test -p cratonvm-native-builtins jca::key_factory::tests -- --nocapture`: 10 passed.
- Exact Spring Boot fixture using the unique release executable:
  - JIT on: `CertificateMatcherTests` PASS, 24 tests, 0 failed, 0 aborted, 0 containers failed (53.201 s).
  - `--nojit`: PASS, 24 tests, 0 failed, 0 aborted, 0 containers failed (59.122 s).

The fixture generates RSA, DSA, Ed25519, Ed448, P-256, and P-521 key pairs;
therefore the validation also confirms this DSA route does not regress the
neighboring real-provider key-generation paths.
