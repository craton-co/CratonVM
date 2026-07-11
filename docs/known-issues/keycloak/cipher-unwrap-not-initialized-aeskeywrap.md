# `Cipher.unwrap()` throws "Cipher not initialized" for AES Key Wrap when used standalone (no prior wrap() on the same instance)

Status: open — likely a `Cipher` state-tracking gap specific to unwrap-only usage

Date observed: 2026-07-10/11 (fresh-binary rerun from current dev, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

`crypto/elytron :: ElytronCryptoJWETest::externalJweAes128KeyWrapTest` is the only failure in an otherwise
fully-passing class (11/12 tests pass, including several *other* JWE encode-then-decode round trips using
different key-management algorithms):

```
=> org.keycloak.jose.jwe.JWEException: org.keycloak.jose.jwe.JWEException: java.lang.IllegalStateException: Cipher not initialized
   org.keycloak.jose.jwe.JWEException.<init>(JWEException.java:33)
   org.keycloak.jose.jwe.JWE.verifyAndDecodeJwe(JWE.java:210)
 Caused by: java.lang.IllegalStateException: Cipher not initialized
   javax.crypto.Cipher.unwrap(Cipher.java:2639)
   org.keycloak.crypto.elytron.AesKeyWrapAlgorithmProvider.decodeCek(AesKeyWrapAlgorithmProvider.java:38)
   org.keycloak.jose.jwe.JWE.getProcessedJWE(JWE.java:197)
```

## Notes

- The test name (`external...`) and the surrounding log (dumps of externally-sourced, pre-computed JWE tokens
  being decoded, not round-tripped within the same test) indicate this specific test method feeds in a
  **fixed/external JWE token** and calls `Cipher.unwrap()` directly on a freshly-obtained `Cipher` instance,
  without a preceding `Cipher.wrap()` call on that same instance in the same test flow.
- Every *other* test method in the same class does an encode-then-decode round trip within the same test,
  meaning (depending on how `AesKeyWrapAlgorithmProvider` obtains/reuses its `Cipher` instance) those paths may
  go through a `wrap()` call first, which apparently leaves the `Cipher` correctly initialized for the subsequent
  `unwrap()`. This specific failing test only calls `unwrap()`, suggesting CratonVM's `Cipher` implementation for
  AES Key Wrap mode may only correctly set its "initialized" state when `init(Cipher.WRAP_MODE, ...)` (or
  `ENCRYPT_MODE`) is called first, and doesn't correctly recognize `init(Cipher.UNWRAP_MODE, ...)` (or
  `DECRYPT_MODE`) alone as sufficient initialization for a subsequent `unwrap()` call.
- Not yet confirmed whether this is specific to AES Key Wrap mode, or would also affect other `Cipher` mode
  combinations under CratonVM when only `UNWRAP_MODE`/`DECRYPT_MODE` is used without a prior
  `WRAP_MODE`/`ENCRYPT_MODE` call on the same instance — worth checking broadly since this is a common,
  legitimate usage pattern (a JWE consumer typically only ever *unwraps*, never wraps, when decoding
  externally-issued tokens).

## Next steps

1. Read `org.keycloak.crypto.elytron.AesKeyWrapAlgorithmProvider.decodeCek()` (line ~38) to see exactly how it
   constructs and initializes its `Cipher` instance before calling `unwrap()`.
2. Search `native-builtins/src/` for CratonVM's `Cipher.init()`/`unwrap()` implementation — check whether the
   "initialized" flag/state is set correctly for `UNWRAP_MODE`/`DECRYPT_MODE` specifically, versus
   `WRAP_MODE`/`ENCRYPT_MODE`.
3. Write a minimal standalone repro: `Cipher c = Cipher.getInstance("AESWrap"); c.init(Cipher.UNWRAP_MODE, key);
   c.unwrap(wrappedKeyBytes, "AES", Cipher.SECRET_KEY);` (no prior `wrap()` call on `c`) under CratonVM, to isolate
   whether this is AES-Key-Wrap-specific or a general unwrap-only-usage gap.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-cipher-unwrap-not-initialized -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.ElytronCryptoJWETest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-20260710.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-v2-20260710-shard1\others-jit\logs\crypto_elytron.org.keycloak.crypto.elytron.test.ElytronCryptoJWETest.out.log`
(1/12 failed: `externalJweAes128KeyWrapTest`), 2026-07-10/11 rerun with a binary built from current `dev`.
