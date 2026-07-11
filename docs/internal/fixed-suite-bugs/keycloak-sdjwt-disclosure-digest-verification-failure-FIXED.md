# FIXED: SD-JWT disclosure-digest verification failures

Status: resolved 2026-07-11.

## Resolution

`Base64.getUrlEncoder().withoutPadding()` stored its configuration in synthetic field slot zero. In real-JDK mode,
that slot is the encoder's `newline` field, so the native encoder lost both the URL-safe alphabet and its
no-padding setting. The encoder now uses the JDK layout (`linemax`, `isURL`, and `doPadding`) and preserves it
when `withoutPadding()` returns a derived encoder.

Verified on the Azure host with a fresh uniquely named release binary:

- native handler regression test: PASS;
- direct SD-JWT-shaped SHA-256 disclosure digest probe: HotSpot and CratonVM both produced
  `B4rHgFoNMPPP0YoisIj52221sNSvj9S4PKUR_XsQSmw` (43 characters, URL-safe, no `=`).

An attempted Maven rerun of the Keycloak classes stops before test discovery on the separate Jansi native-method
gap (`org.fusesource.jansi.internal.CLibrary.init()`), which is unrelated to SD-JWT. The direct probe executes the
same `SHA-256` plus `Base64.getUrlEncoder().withoutPadding().encodeToString(...)` path responsible for this issue.

## Historical report

Status: open — **root cause confirmed**: same underlying bug as
`base64-urlencoder-withoutpadding-not-stripping-padding.md` in this same folder. High impact within SD-JWT/OID4VC
test coverage.

**Update**: `SdJwtUtils.encodeNoPad()` (the exact method used to compute disclosure digests here) is confirmed to
call `Base64Url.encode(bytes)` → `Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)` — the identical
code path shown to produce an extra, un-stripped padding character in
`base64-urlencoder-withoutpadding-not-stripping-padding.md` (proven there via `ElytronPemUtilsTest`'s
thumbprint-length assertions failing by exactly +1 character). A digest computed with a stray trailing `=` will
never string-match the (correctly-unpadded) digest values embedded in a valid SD-JWT's `_sd` claims, which
exactly matches the "at least one disclosure is not protected by digest" symptom below — including for
legitimate, well-formed tokens. Fixing the Base64 `withoutPadding()` bug should resolve this doc's failures too;
kept as a separate doc since it's a distinct, high-value symptom worth tracking its own verification.

Date observed: 2026-07-10/11 (fresh-binary rerun from current dev, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Summary

`org.keycloak.crypto.elytron.test.sdjwt.ElytronCryptoSdJwtVPVerificationTest` (and the FIPS1402 equivalent,
`FIPS1402SdJwtVPVerificationTest`) fail **18 of 24** test methods. Critically, this includes multiple tests whose
names make clear they're meant to *succeed* — `testVerif_s20_1_sdjwt_with_kb`, `testVerifKeyBindingNotRequired`,
`testVerif_s20_8_sdjwt_with_kb__CnfRSA`, `testVerif_s20_8_sdjwt_with_kb__AltCnfCurves` — all fail with:

```
org.keycloak.common.VerificationException: At least one disclosure is not protected by digest
  org.keycloak.common.VerificationException.<init>(VerificationException.java:32)
  org.keycloak.sdjwt.SdJwtVerificationContext.validateDisclosuresVisits(SdJwtVerificationContext.java:555)
  org.keycloak.sdjwt.SdJwtVerificationContext.validateDisclosuresDigests(SdJwtVerificationContext.java:312)
  org.keycloak.sdjwt.SdJwtVerificationContext.verifyIssuance(SdJwtVerificationContext.java:122)
```

The remaining failures (12, mostly named `testShouldFail_*`) are `org.junit.ComparisonFailure` — these are
negative tests that expect a *specific* failure mode but got a different one (very plausibly the exact same
digest-mismatch exception firing where a different, more specific exception was expected — i.e. one root cause
manifesting as two different symptom categories depending on whether the test itself already expected *some*
failure).

## Root cause hypothesis — confirmed code path, root defect not yet pinned to a specific CratonVM subsystem

`SdJwtVerificationContext.validateDisclosuresVisits()` (line 552-557):

```java
private void validateDisclosuresVisits(Set<String> visitedDisclosureStrings) throws VerificationException {
    if (visitedDisclosureStrings.size() < disclosures.size()) {
        throw new VerificationException("At least one disclosure is not protected by digest");
    }
}
```

This fires when not every disclosure string in the SD-JWT could be matched to a digest referenced in the
payload's `_sd` claims during recursive traversal (`validateViaRecursiveDisclosing`). The match is computed via:

```java
// SdJwtVerificationContext.computeDigestDisclosureMap (line 87-93)
String digest = SdJwtUtils.hashAndBase64EncodeNoPad(disclosureString.getBytes(), issuerSignedJwt.getSdHashAlg());
```

Note: this calls `disclosureString.getBytes()` — **the no-argument overload, using the JVM's *default*
charset** — not the explicit-UTF-8 `SdJwtUtils.utf8Bytes()` helper that exists right next to it in the same
utility class and is used elsewhere. Since JEP 400 (JDK 18+), the JVM's default charset is specified to be UTF-8
regardless of platform, but this is exactly the kind of platform/default-charset resolution detail that's easy
for an alternative JVM implementation to get subtly wrong (e.g. defaulting to the OS's native codepage on Windows
instead of UTF-8, or some other charset-negotiation gap). If CratonVM's default charset differs from UTF-8 for
any of the disclosure strings involved (which are typically base64url-encoded JSON, but the *hash input* here is
the *raw* disclosure string bytes, not necessarily always pure 7-bit ASCII depending on what's embedded), the
computed digest would differ from the value referenced in the token's `_sd` claims (which were presumably
computed with a correct/expected UTF-8 encoding when the test's SD-JWT fixtures were generated), causing the
"not protected by digest" mismatch — even for entirely valid, well-formed tokens.

This is a hypothesis, not yet confirmed by direct comparison — see Next steps.

## Next steps

1. Add a temporary print/log of the exact bytes (`disclosureString.getBytes()`) vs
   `disclosureString.getBytes(StandardCharsets.UTF_8)` for one of the failing positive test cases
   (`testVerif_s20_1_sdjwt_with_kb` is a small, single-disclosure case, good for isolation) under CratonVM — if
   they differ, this confirms the default-charset hypothesis directly and pinpoints exactly which characters
   diverge.
2. Alternatively/additionally, check `HashUtils.hash(hashAlg, bytes)` (the actual digest computation) against a
   known-good SHA-256 test vector under CratonVM to rule out a broader digest-computation bug (less likely given
   how much other crypto/JWT/JWE work correctly elsewhere in this same run, but worth a quick isolated check
   before concluding it's charset-only).
3. Once root-caused, verify the fix against all 24 tests in `SdJwtVPVerificationTest`-based classes (this base
   test class is shared/parameterized across `ElytronCryptoSdJwtVPVerificationTest` and
   `FIPS1402SdJwtVPVerificationTest`, so a fix here should resolve both simultaneously) — check whether the
   `testShouldFail_*` ComparisonFailure group also clears up once the underlying digest computation is corrected
   (they may need the digest check to *pass* first before reaching their own intended negative-path assertion).

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-sdjwt-digest -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.sdjwt.ElytronCryptoSdJwtVPVerificationTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-20260710.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-v2-20260710-shard1\others-jit\logs\crypto_elytron.org.keycloak.crypto.elytron.test.sdjwt.ElytronCryptoSdJwtVPVerificationTest.out.log`
(18/24 failed) and the FIPS1402 sibling
`crypto_fips1402.org.keycloak.crypto.fips.test.sdjwt.FIPS1402SdJwtVPVerificationTest.out.log`, 2026-07-10/11
rerun with a binary built from current `dev`. Source:
`apps/keycloak/core/src/main/java/org/keycloak/sdjwt/{SdJwtVerificationContext,SdJwtUtils}.java`.
