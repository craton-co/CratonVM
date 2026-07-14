# EdDSA Ed448 SD-JWT key-binding `ComparisonFailure` — root-caused and FIXED

Status: fixed
Observed: 2026-07-13 (narrower residual after `eddsa-keyspec-to-publickey-invalidkeyspecexception` was fixed)
Fixed: 2026-07-14

## Root cause

Not an EdDSA/Ed448-specific bug at all. Two unrelated, pre-existing bootstrap regressions in real-JDK
mode combined to produce this exact symptom:

1. **`java.util.Locale` early-touch regression** (`InternalError: null property: java.home`, JDK 25's
   `StaticProperty.<clinit>` reading `System.getProperties().getProperty("java.home")`) — already
   root-caused and fixed upstream on `dev` (`f62d20736`, `68c44f62d`): `register_properties_sidetable`'s
   native overrides for `java.util.Properties` inherited whatever `current_category` happened to be
   ambient at each of its call sites instead of pinning their own, so `set_drop_synthetic_stubs(true)`
   (real-JDK mode) silently dropped them once the ambient category was `SyntheticStub`. This blocked
   *any* real-JDK program that touches `Locale` early — including `sun.security.util.KnownOIDs.<clinit>`,
   reached from constructing an `EdDSA KeyPairGenerator` for either curve.

2. **`String.getBytes()` / `getBytes(String)` / `getBytes(Charset)` returning a zero-length array** — same
   bug family, independently found and documented by a concurrent session as
   [`string-getbytes-empty-real-jdk-mode-FIXED.md`](string-getbytes-empty-real-jdk-mode-FIXED.md) and fixed
   there (`d9e693d7a`, merged `dev` via `ed46a682f`). This investigation hit the identical symptom
   independently (own standalone repro, `GetBytesProbe.java`, `"hello".getBytes()` → `len=0` on the same
   `register_real_charset_natives` never pinning its own category) before discovering the fix had already
   landed upstream; ended up rebasing onto it rather than duplicating the code change. `"any
   string".getBytes()` (any overload) returned a **zero-length array with no exception** — nothing to do
   with Keycloak, JWK, or crypto by itself.

Effect on this test: `KeyBindingJWT.verifySignature` and the JWT compact-serialization logic call
`String.getBytes()` to turn the signing input into bytes. With that returning `b""` regardless of the
actual content, `Signature.update(msg)` always accumulated zero bytes — for both the correctly-signed
and the deliberately-tampered key-binding JWT — so both signed identically (over the empty message) and
the intentionally-invalid-signature negative-test case (`SdJwtKeyBindingTest.java:192`,
`Assert.assertEquals("Key binding JWT invalid", ve.getMessage())`) never reached its expected
`VerificationException`, instead falling through to a later check with a different message → the
observed `ComparisonFailure`. **This reproduced identically for Ed25519** at the raw
`java.security.Signature` level (verified via a standalone `Ed448Probe.java` exercising both curves
directly, no Keycloak involved) — it was never Ed448-specific; SD-JWT's Ed25519 test happened not to
exercise a `getBytes()` call site sensitive to this in the same way, or exercised it via a path that
still failed differently before the java.home fix landed (masking a hard crash instead of a
`ComparisonFailure`).

## Fix

Both halves are already fixed and merged on `dev` — no code change from this investigation was needed
(a `charset.rs` fix written independently here turned out to be byte-for-byte the same shape as
`d9e693d7a`'s already-merged fix, so it was dropped in favor of rebasing):

- `f62d20736` / `68c44f62d` (`java.util.Properties` / `CopyOnWriteArrayList` — the java.home/Locale half)
- `d9e693d7a` (`native-builtins/src/charset.rs`, `register_real_charset_natives` — the getBytes half),
  merged via `ed46a682f`

Both wrap the affected `register_*` function's entire body in
`registry.with_category(NativeKind::Bridge, |registry| { ... })` so its natives no longer depend on the
caller's ambient category state.

## Verification (this investigation, on top of the merged fixes)

- `GetBytesProbe.java` (`"hello".getBytes()`): `len=0` → `len=5` (correct), byte values match ASCII.
- `Ed448Probe.java` (standalone `java.security.Signature`, both Ed25519 and Ed448, no Keycloak): before
  the fixes, `verify(tamperedContent, signatureOverOriginalContent)` incorrectly returned `true` for BOTH
  curves (both were signing/verifying over `b""`); after, `verify()` correctly returns `false` for
  tampered content and `true` for the original, for both curves.
- `crypto/fips1402 :: FIPS1402SdJwtKeyBindingTest` — full class, all 7 tests (Ed25519, Ed448, EC P-256/384/521,
  RSA 2048/4096 key binding): `OK (7 tests)`.
- `crypto/elytron :: ElytronCryptoSdJwtKeyBindingTest` — full class, same 7 tests: `OK (7 tests)`.

This is the first confirmation that the getBytes/Locale fixes actually close the literal SD-JWT
Ed448/Ed25519 key-binding regression this doc was originally opened for — the other two docs found and
fixed the underlying bug via unrelated symptoms (HTTP server body / Spring test bootstrap) without
running these specific Keycloak test classes.

## Repro (superseded)

The original repro command referenced a since-cleaned-up worktree/binary
(`CratonVM-keycloak-nonpassed-v2-20260710`, `cratonvm-nonpassed-v2-refresh2-20260712.exe`). Current repro:
build `cratonvm.exe` from current `dev`, then:

```
cd apps/keycloak
./mvnw -q -pl crypto/fips1402 -am -DskipTests -Dcheckstyle.skip -Dformat.skip -Dspotbugs.skip test-compile \
  org.apache.maven.plugins:maven-dependency-plugin:3.6.1:build-classpath -Dmdep.outputFile=cp.txt -Dmdep.includeScope=test
# classpath = crypto/fips1402/{target/classes,target/test-classes} ; core/{target/classes,target/test-classes} ;
#             common/target/classes ; $(cat crypto/fips1402/cp.txt)
cratonvm.exe --java-home <jdk25> -c "<classpath>" org.junit.runner.JUnitCore \
  org.keycloak.crypto.fips.test.sdjwt.FIPS1402SdJwtKeyBindingTest
```
