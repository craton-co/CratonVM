# Keycloak suite — CratonVM crash/bug reports

Harness: `test-infra/run-keycloak-suite-full.sh` (one VM per concrete `*Test`, CratonVM
then HotSpot, across modules **core, crypto/default, common, server-spi, server-spi-private** —
92 concrete classes). Branch `fix/keycloak-suite-loop`, worktree `C:/craton/CratonVM-kcsuite`.

## Environment caveat (important)
This box runs **many concurrent agent sessions**. A peer session's
`taskkill //F //IM cratonvm.exe` will kill an unrelated running test → spurious
`rc=1` / 0-byte "silent crash". The harness was changed to launch a **uniquely-named
binary** (`cratonvm-kc.exe`) and only kill that name. Re-running clean confirmed
**7 of the 21 baseline "failures" were contamination** (e.g. SkeletonKeyTokenTest,
IDTokenTest, TotpTest, SessionExpirationUtilsTest, IdentityBrokerStateTest — all PASS
clean). Those are **not** CratonVM bugs and get no report.

## Authoritative result (clean run, contamination-robust)
- 92 concrete classes; **57 PASS / 35 FAIL** on CratonVM (clean baseline).
- Of the 35 FAILs, **21 also fail on HotSpot** (pre-existing / harness-classpath, ignored per request).
- **14 are CratonVM-only** (HotSpot PASS). Grouped by root cause below.

## After fixes (iteration 2, commit `96a89347`)
- **59 PASS / 33 FAIL**, **12 CratonVM-only** failures (down from 14). **Zero regressions.**
- Fixed: CredentialModelTest, CredentialModelBackwardsCompatibilityTest (now green).
- Improved internally (cascade removed, still FAIL on RSA-key #11 follow-ons): the 4 BC
  cert classes.
- Remaining 12 are dominated by the **synthetic-RSA-key** umbrella (#11, architectural)
  plus ECDH (#04), EC keyspec (#05), Cipher initLock (#03), stream close-handlers (#09),
  JSON compare (#08) — each documented; full green is gated on the RSA→real-provider
  routing, scoped out here to avoid VM-wide regression risk.

| # | Report | Classes affected | Status |
|---|--------|------------------|--------|
| 01 | [Hashtable.keys()/elements() wrong Entry layout](01-hashtable-enumeration-real-entry-layout.md) | DefaultCertificateIdentityExtractorTest, DefaultCryptoJWKTest, DefaultCryptoRSAVerifierTest, PemUtilsBCTest | **FIXED** (cascade removed; classes still fail on RSA-key #11 / CN-extraction follow-on) |
| 02 | [Constructor.getGenericParameterTypes() null for primitive param](02-constructor-generic-primitive-null.md) | CredentialModelTest, CredentialModelBackwardsCompatibilityTest | **FIXED ✓ both classes green** |
| 11 | [Synthetic RSA keys are not real RSAPublicKey (umbrella)](11-rsa-synthetic-key-not-rsapublickey.md) | RSAVerifier, JWKT, KeyPairVerifier, SdJwtVP, JWKSUtils, PemUtilsBC | open (architectural; gates 06/07/10) |
| 03 | [Cipher.chooseProvider monitorenter NPE (null lock)](03-cipher-chooseprovider-monitorenter-npe.md) | DefaultCryptoJWETest | open |
| 04 | [ECDH KeyAgreement "Algorithm ECDH not available"](04-ecdh-keyagreement-not-available.md) | BCEcdhEsAlgorithmProviderTest | open |
| 05 | [BC ECPublicKeySpec InvalidKeySpecException](05-bc-ecpublickeyspec-invalidkeyspec.md) | BCECDSACryptoProviderTest | open |
| 06 | [KeyPairVerifier "Keys don't match" / decode private key](06-keypair-verifier-decode.md) | DefaultCryptoKeyPairVerifierTest | open |
| 07 | [SD-JWT VP "Could not process cnf/jwk"](07-sdjwt-vp-cnf-jwk.md) | DefaultCryptoSdJwtVPVerificationTest | open |
| 08 | [StripSecretsUtils JSON ComparisonFailure](08-stripsecrets-json-comparison.md) | StripSecretsUtilsTest | open |
| 09 | [StreamsUtil onClose / auto-close propagation](09-streamsutil-onclose-propagation.md) | StreamsUtilTest | open |
| 10 | [DefaultCryptoJWKSUtilsTest 1 failure](10-jwksutils-one-failure.md) | DefaultCryptoJWKSUtilsTest | open |
