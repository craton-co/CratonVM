# EdDSA Ed448 (but not Ed25519) SD-JWT key-binding test still fails after the KeySpec fix — narrower residual

Status: open — narrower residual after `eddsa-keyspec-to-publickey-invalidkeyspecexception` was fixed; Ed25519 now
passes, Ed448 specifically still fails, with a different (comparison-based) symptom

Date observed: 2026-07-13 (second refresh rerun against non-passed-before classes, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

`crypto/fips1402 :: FIPS1402SdJwtKeyBindingTest::testEdDSAKeyBindingWithEd448` still fails, but with a different
signature than the previously-fixed `InvalidKeySpecException`:

```
=> org.junit.ComparisonFailure:
   org.keycloak.sdjwt.sdjwtvp.SdJwtKeyBindingTest.testKeyBinding(SdJwtKeyBindingTest.java:192)
   org.keycloak.sdjwt.sdjwtvp.SdJwtKeyBindingTest.testEdDSAKeyBindingWithEd448(SdJwtKeyBindingTest.java:70)
KCRUNNER_RESULT tests=7 failed=1 aborted=0 skipped=0 containersFailed=0
```

The sibling `testEdDSAKeyBindingWithEd25519` in the same class now passes cleanly (confirming
`eddsa-keyspec-to-publickey-invalidkeyspecexception` is genuinely fixed for Ed25519) — this is specific to Ed448.

## Notes

- `ComparisonFailure` (no message body captured in the summary output) means an actual vs expected string/value
  mismatch somewhere in `SdJwtKeyBindingTest.testKeyBinding` (line 192) — likely a signature, hash, or JWT
  compact-serialization string that doesn't match for the Ed448 curve specifically, distinct from the earlier
  hard failure at key-material construction time.
- Given Ed25519 now works correctly end-to-end (key generation, KeySpec reconstruction, AND signing/verification
  all succeed), the remaining gap is narrower — likely something Ed448-curve-specific (different key/signature
  sizes, a curve-specific constant, or a still-incomplete part of CratonVM's Ed448 support that Ed25519 doesn't
  exercise).

## Next steps

1. Get the full `ComparisonFailure` message (expected vs actual values) — the harness's summary output
   didn't include it in this run; re-run with more verbose output or check the full JUnit XML/console output for
   this specific test to see exactly what differs.
2. Compare CratonVM's Ed448 vs Ed25519 signing/verification code paths (`native-builtins/src/jca/signature.rs` or
   wherever EdDSA signing lives) for anything hardcoded to Ed25519's specific key/signature sizes that wouldn't
   generalize correctly to Ed448 (Ed448 keys/signatures are larger: 57-byte keys vs Ed25519's 32-byte, 114-byte
   signatures vs 64-byte).

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-ed448-keybinding -ClassList <(printf 'module\tclass\ncrypto/fips1402\torg.keycloak.crypto.fips.test.sdjwt.FIPS1402SdJwtKeyBindingTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh2-20260712.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh2-shard1\all-jit\logs\crypto_fips1402.org.keycloak.crypto.fips.test.sdjwt.FIPS1402SdJwtKeyBindingTest.out.log`,
2026-07-13 rerun with a binary built from current `dev`.
