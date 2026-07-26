# Resolved: Keycloak Elytron `AESWrap_128` `Cipher.wrap` / `Cipher.unwrap`

Status: resolved on 2026-07-12

## Original symptom

Keycloak Elytron's JWE A128KW paths reached CratonVM's native `Cipher`
implementation but both public key-wrapping operations threw explicit
`IllegalStateException`s:

```
Cipher.unwrap not implemented for AESWrap_128
Cipher.wrap not implemented for AESWrap_128
```

The affected Keycloak classes were
`ElytronCryptoJWETest::externalJweAes128KeyWrapTest` and
`ElytronEcdhEsAlgorithmProviderTest`.

## Resolution

`../../../../native-builtins/src/jca/cipher.rs` now registers `Cipher.wrap(Key)` and
`Cipher.unwrap(byte[], String, int)` for native Cipher instances. For the
`AESWrap` transformations this implements RFC 3394 AES Key Wrap using the
existing AES block primitive, enforces the named `AESWrap_128` key-encryption
key size, checks the RFC integrity value during unwrap, and reconstructs a
`SecretKeySpec` for `Cipher.SECRET_KEY`.

## Validation

- `cargo test -p cratonvm-native-builtins jca::cipher::tests::aes_key --lib`
  passed the RFC 3394 section 4.1 known-answer vector and tamper rejection.
- A release CratonVM binary built on the Azure host ran
  `../../../../apps/keycloak-suite-runner/AesWrap128Probe.java` with JDK 21 and printed
  `AES_WRAP_128_OK`. The probe verifies the exact RFC ciphertext and an
  `AESWrap_128` wrap/unwrap round trip through the public JCA methods.
- The retained Keycloak runner fixture no longer contains either compiled
  Elytron test class, so its attempted rerun ends in `ClassNotFoundException`.
  That fixture state is not used as pass evidence; the focused runtime probe
  covers the same A128KW contract directly.
