# `java.util.Base64.Encoder.withoutPadding()` does not strip the padding character — core JDK API bug, likely explains the SD-JWT digest-verification failures too

Status: open — high-confidence, foundational CratonVM bug in a core, extremely widely-used JDK API (not Keycloak-specific)

Date observed: 2026-07-10/11 (fresh-binary rerun from current dev, branch fix/keycloak-nonpassed-rerun-v2-20260710)

## Resolution

Fixed in July 2026. CratonVM's Base64 intrinsics were registered but their
encoder configuration had two defects: concrete JDK methods were not forced
through the native override path, and the intrinsic stored its configuration in
synthetic slot 0. In the real JDK `Base64$Encoder`, slot 0 is the `newline`
reference; the actual `linemax`, `isURL`, and `doPadding` fields occupy slots
1, 2, and 3. The VM now dispatches the Base64 API family to the intrinsics and
uses that real layout. Remote VM probes verify URL-safe output (`-_8=`),
unpadded output (`-_8`), SHA-1 length 27, SHA-256 length 43, and preservation
of normal padded output.

## Summary

`crypto/elytron :: ElytronPemUtilsTest` fails both of its Base64URL-thumbprint-length tests by exactly one
character:

```
testGenerateThumbprintSha256: java.lang.AssertionError: expected:<43> but was:<44>
testGenerateThumbprintSha1:   java.lang.AssertionError: expected:<27> but was:<28>
```

Both numbers are off by exactly **+1**, and both are the classic "still has the trailing padding character"
signature: a SHA-256 digest is 32 bytes → Base64-with-padding is 44 characters, Base64-**without**-padding
(RFC 4648 §5, used for JWK thumbprints per RFC 7638) is 43 characters (the last `=` pad character dropped). A
SHA-1 digest is 20 bytes → 28 chars padded, 27 unpadded. The test expects the correctly-unpadded lengths (43, 27)
but CratonVM produces the *padded* lengths (44, 28) — i.e. **the padding character is not actually being
stripped**, despite `withoutPadding()` being explicitly requested.

## Root cause — traced directly to the JDK's own `java.util.Base64` API, not any Keycloak code

The call chain is entirely standard JDK API, no CratonVM-specific or third-party wrapper beyond a thin pass-through:

```java
// apps/keycloak/common/src/main/java/org/keycloak/common/util/Base64Url.java
public static final Base64.Encoder BASE64_URL_ENCODER_WITHOUT_PADDING = Base64.getUrlEncoder().withoutPadding();

public static String encode(byte[] bytes) {
    return BASE64_URL_ENCODER_WITHOUT_PADDING.encodeToString(bytes);
}
```

`java.util.Base64.getUrlEncoder().withoutPadding()` is the **standard JDK** mechanism for RFC-4648-compliant
unpadded base64url encoding — nothing about this is Keycloak-specific. Since this reproduces via a completely
ordinary `Base64.Encoder.withoutPadding().encodeToString(...)` call, this is a genuine defect in CratonVM's own
`java.util.Base64` implementation: `withoutPadding()` either isn't being honored at all (encoder always pads), or
strips too few characters, or the "without padding" configuration isn't correctly threaded through to
`encodeToString`.

## Why this is likely the SAME root cause behind the SD-JWT digest-verification failures

See `sdjwt-disclosure-digest-verification-failure.md` in this same folder — 18/24 tests in
`ElytronCryptoSdJwtVPVerificationTest`/`FIPS1402SdJwtVPVerificationTest` fail with
`VerificationException: At least one disclosure is not protected by digest`, including tests meant to *succeed*.
SD-JWT disclosure digests are computed via `SdJwtUtils.hashAndBase64EncodeNoPad(...)`, which almost certainly
also routes through `Base64.getUrlEncoder().withoutPadding()` (or an equivalent no-pad encoder) per the SD-JWT
spec (RFC 9701 requires base64url-**no-padding** for disclosure digests). If CratonVM's no-pad encoder leaves a
trailing `=` in, every computed digest would have an extra character compared to the digest values embedded in a
correctly-generated SD-JWT's `_sd` claims — the string comparison would never match, producing exactly the
"at least one disclosure is not protected by digest" symptom seen there, **even for entirely valid, well-formed
tokens**. This single Base64 bug plausibly explains both findings at once, and probably other stray failures
elsewhere in the suite that involve base64url-no-pad encoding (JWS/JWT compact serialization, JWK thumbprints,
PKCE code challenges, etc. — all commonly use unpadded base64url per their respective specs).

## Impact

`java.util.Base64` is one of the most widely-used APIs in the entire JDK — used throughout JWT/JOSE, TLS
certificate handling, HTTP Basic auth, and countless other places, essentially anywhere binary data needs a safe
text encoding. A broken `withoutPadding()` has potential impact far beyond Keycloak or even this specific test
suite. Given the extremely high confidence and small, well-isolated repro (see below), this is one of the
highest-priority, easiest-to-fix findings from this entire investigation.

## Next steps

1. Search `native-builtins/src/` (or wherever `java.util.Base64`/`Base64.Encoder` is implemented) for the
   `withoutPadding()` / padding-stripping logic and fix it directly — this should be a small, contained fix given
   how precisely isolated the symptom is (always exactly one extra trailing `=` character).
2. Verify the fix with the minimal repro below, then re-run `ElytronPemUtilsTest` (expect both thumbprint tests
   to pass) and the SD-JWT verification classes (expect most/all of the 18 currently-failing tests in
   `SdJwtVPVerificationTest` to also start passing, confirming the shared root cause).
3. Given the potential breadth of impact, consider a quick standalone sweep of any test suite that round-trips
   base64url-no-pad data (JWT compact serialization is the most likely to have silently-passing-anyway false
   negatives if both sides of a comparison independently use the same buggy encoder).

## Repro

Minimal standalone repro (no Keycloak needed):

```java
import java.util.Base64;
public class B64Repro {
    public static void main(String[] args) {
        byte[] sha256 = new byte[32]; // any 32-byte input, e.g. a real SHA-256 digest
        String encoded = Base64.getUrlEncoder().withoutPadding().encodeToString(sha256);
        System.out.println("length=" + encoded.length() + " (expected 43)");
        System.out.println("ends with '=': " + encoded.endsWith("="));
    }
}
```
Run under CratonVM vs real HotSpot and compare `encoded.length()` / whether it ends in `=`.

Keycloak-level repro:
```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-base64-withoutpadding -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.ElytronPemUtilsTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-20260710.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-v2-20260710-shard1\others-jit\logs\crypto_elytron.org.keycloak.crypto.elytron.test.ElytronPemUtilsTest.out.log`,
2026-07-10/11 rerun with a binary built from current `dev`. Source:
`apps/keycloak/common/src/main/java/org/keycloak/common/util/Base64Url.java` (line 30),
`apps/keycloak/common/src/main/java/org/keycloak/common/crypto/PemUtilsProvider.java` (line 145-147).
