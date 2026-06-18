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
- Remaining failures dominated by the **synthetic-RSA-key** umbrella (#11)
  plus ECDH (#04), EC keyspec (#05), Cipher initLock (#03), stream close-handlers (#09),
  JSON compare (#08).

## Latest (iteration 9) — **66 PASS / 26 FAIL, 5 CratonVM-only failures, 0 regressions**
**Nine** classes flipped fully green over the effort, via fundamental VM fixes (each
verified with no regressions across the 92-class suite):
- **DefaultCryptoJWKTest** — P-384/P-521 EC JIT-miscompile fix (#17, `ca34440d`).
- **CredentialModelTest, CredentialModelBackwardsCompatibilityTest** — primitive generic
  param mirror (#02).
- **DefaultCryptoRSAVerifierTest** — real RSA keys, `route_rsa_to_real` (#11, `4b2560a0`).
- **DefaultCryptoJWKSUtilsTest** — EC cert `getPublicKey` + `RSAPublicKeySpec` import (#12, `7c98ec8f`).
- **StripSecretsUtilsTest** — `Map.Entry.setValue()` write-through (#13, `bfbb8093`).
- **PemUtilsBCTest, DefaultCryptoKeyPairVerifierTest** — PKCS#1 RSA private key + imported-key
  sign bridge (#15, `b1a6fa4b`); cert-verify OID resolution (#14, `7010eee7`).
- **DefaultCertificateIdentityExtractorTest** — X500 DN RFC2253 spacing + multi-valued RDN
  decode (#16, `74fa043c`).
Plus the Hashtable-`$Entry`-enumeration fix (#01).

Remaining 5 are deeper, mostly-independent issues: RSA-OAEP `Cipher`
(#03), ECDH (#04), BC `ECPublicKeySpec`
(#05), SD-JWT RSA cnf/jwk (#07), cert subject parsing (CertExtractor), stream `onClose` (#09):
RSA-OAEP `Cipher` (#03), ECDH (#04), BC `ECPublicKeySpec` (#05), imported RSA private keys
(#06), SD-JWT RSA cnf/jwk (#07), BC `getRDNs` CN extraction (#01 tail), and stream
`onClose` handlers (#09).

| # | Report | Classes affected | Status |
|---|--------|------------------|--------|
| 01 | [Hashtable.keys()/elements() wrong Entry layout](01-hashtable-enumeration-real-entry-layout.md) | DefaultCertificateIdentityExtractorTest, DefaultCryptoJWKTest, DefaultCryptoRSAVerifierTest, PemUtilsBCTest | **FIXED** (cascade removed; classes still fail on RSA-key #11 / CN-extraction follow-on) |
| 02 | [Constructor.getGenericParameterTypes() null for primitive param](02-constructor-generic-primitive-null.md) | CredentialModelTest, CredentialModelBackwardsCompatibilityTest | **FIXED ✓ both classes green** |
| 11 | [Synthetic RSA keys are not real RSAPublicKey (umbrella)](11-rsa-synthetic-key-not-rsapublickey.md) | RSAVerifier ✓, JWKT, KeyPairVerifier, SdJwtVP, JWKSUtils, PemUtilsBC | **FIXED (key-type)** route_rsa_to_real (4b2560a0); remaining reds are separate bugs |
| 03 | [Cipher.chooseProvider monitorenter NPE (null lock)](03-cipher-chooseprovider-monitorenter-npe.md) | DefaultCryptoJWETest | **FIXED** — RSA-OAEP/PKCS1 Cipher + AES-GCM getOutputSize/4-arg doFinal; JWE 11/11 (both RSA modes) |
| 04 | [ECDH KeyAgreement "Algorithm ECDH not available"](04-ecdh-keyagreement-not-available.md) | BCEcdhEsAlgorithmProviderTest | **FIXED** — KeyAgreement→SunEC ECDH SPI + identity (n·P) order-check fix; 2/2 (incl. ECDH-ES JWE) |
| 05 | [BC ECPublicKeySpec InvalidKeySpecException](05-bc-ecpublickeyspec-invalidkeyspec.md) | BCECDSACryptoProviderTest | **FIXED** — BC-spec→BC KeyFactory SPI + provider-aware EC keygen (BC keys when "BC" requested); 3/3, 0 regressions |
| 06 | [KeyPairVerifier "Keys don't match" / decode private key](06-keypair-verifier-decode.md) | DefaultCryptoKeyPairVerifierTest | open |
| 07 | [SD-JWT VP "Could not process cnf/jwk"](07-sdjwt-vp-cnf-jwk.md) | DefaultCryptoSdJwtVPVerificationTest | **FIXED** — synthetic RSAPublicKeySpec import + RSA-PSS (PS256/384/512) verify; SdJwtVP 24/24 (both RSA modes) |
| 08 | [StripSecretsUtils JSON ComparisonFailure](08-stripsecrets-json-comparison.md) | StripSecretsUtilsTest | **FIXED** via #13 (Map.Entry.setValue) |
| 09 | [StreamsUtil onClose / auto-close propagation](09-streamsutil-onclose-propagation.md) | StreamsUtilTest | open (stream close-handler feature) |
| 10 | [DefaultCryptoJWKSUtilsTest 1 failure](10-jwksutils-one-failure.md) | DefaultCryptoJWKSUtilsTest | **FIXED** via #12 |
| 12 | [EC cert getPublicKey null (BC converter)](12-ec-cert-getpublickey-null.md) | DefaultCryptoJWKTest, JWKSUtils ✓ | **FIXED (getPublicKey)** `7c98ec8f`; EC-cert ECDSA-verify remains |
| 13 | [Map.Entry.setValue no write-back](13-map-entry-setvalue-no-writeback.md) | StripSecretsUtilsTest ✓ (+ all entrySet RMW) | **FIXED** `bfbb8093` |
| 14 | [X509Certificate.verify() sig-alg OID unresolved](14-cert-verify-signature-oid.md) | DefaultCryptoJWKTest (EC cert verify; + all EC/RSA cert.verify) | **FIXED** `7010eee7`; JWKT 4→2 (rest = separate P-384/P-521 EC gap) |
| 17 | [P-384/P-521 EC + cert.verify SignatureUtil null](17-ec-p384-p521-progressive-corruption.md) | DefaultCryptoJWKTest (publicEs256P*, EC cert-gen) | **FIXED** — intpoly JIT ban (`ca34440d`) + SignatureUtil/SharedSecrets intercept; JWKT 10/10 |
