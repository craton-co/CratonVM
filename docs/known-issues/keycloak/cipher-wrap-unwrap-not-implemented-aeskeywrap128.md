# `Cipher.wrap`/`Cipher.unwrap` explicitly "not implemented" for AESWrap_128

Status: open — narrower successor to the previously-fixed "Cipher not initialized" AES-Key-Wrap bug; the
initialization-state bug is fixed, but the underlying wrap/unwrap operation for this specific algorithm variant
is now clearly reported as genuinely unimplemented

Date observed: 2026-07-11 (refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

Two `crypto/elytron` classes fail with an explicit "not implemented" `IllegalStateException` from CratonVM's own
`Cipher` implementation (not a generic JDK exception — the message format is CratonVM's own diagnostic):

```
crypto/elytron :: ElytronCryptoJWETest::externalJweAes128KeyWrapTest
=> java.lang.IllegalStateException: Cipher.unwrap not implemented for AESWrap_128
   org.keycloak.crypto.elytron.AesKeyWrapAlgorithmProvider.decodeCek(AesKeyWrapAlgorithmProvider.java:38)

crypto/elytron :: ElytronEcdhEsAlgorithmProviderTest
=> java.lang.IllegalStateException: Cipher.wrap not implemented for AESWrap_128
```

## Notes

- This is a **different, more specific** symptom than the previously-fixed
  `cipher-unwrap-not-initialized-aeskeywrap.md`/`keycloak-cipher-unwrap-aeskeywrap.md` finding from the prior
  investigation pass (that one was `IllegalStateException: Cipher not initialized`, a state-tracking bug that
  masked the real gap). Now that the initialization bug is fixed, both `wrap` and `unwrap` cleanly report
  themselves as **not implemented** for the `AESWrap_128` algorithm variant specifically.
- Both directions (wrap and unwrap) are affected, in two different test classes, confirming this isn't a
  one-off — `AESWrap_128` key-wrapping simply isn't implemented in CratonVM's `Cipher` yet.

## Next steps

1. Search `native-builtins/src/jca/cipher.rs` (per this session's earlier finding of that file's existence) for
   where `Cipher.wrap`/`unwrap` are implemented and check the algorithm dispatch table for `AESWrap`/`AESWrap_128`
   — likely a genuinely missing registration/implementation branch rather than a bug in existing logic.
2. Implement AES Key Wrap (RFC 3394) wrap/unwrap if not already present elsewhere in the codebase to borrow from.
3. Verify against both affected classes once implemented.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-aeswrap128-not-implemented -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.ElytronCryptoJWETest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh-20260711.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh-shard1\all-jit\logs\crypto_elytron.org.keycloak.crypto.elytron.test.{ElytronCryptoJWETest,ElytronEcdhEsAlgorithmProviderTest}.out.log`,
2026-07-11 refresh rerun with a binary built from current `dev`.
