# Keycloak suite — CratonVM vs HotSpot

**Date:** 2026-06-10T19:31:15Z
**CratonVM:** `C:/craton/CratonVM/target/release/cratonvm.exe`  (HEAD efd17806)
**HotSpot:** `C:/Program Files/Java/jdk-25/bin/java.exe` (java version 25.0.1 2025-10-21 LTS)
**Per-class timeout:** 240s   **Logs:** `C:/craton/CratonVM/test-infra/suite-results/keycloak-20260610-162332/`

| Module | Test class | CratonVM | tests | HotSpot | CV time | HS time | Slowdown |
|--------|-----------|----------|-------|---------|---------|---------|----------|
| core | org.keycloak.AtHashTest | FAIL | 0 | PASS | 1.2s | 0.5s | 2.4x |
| core | org.keycloak.HashTest | PASS | 1 | PASS | 1.7s | 0.5s | 3.4x |
| core | org.keycloak.JsonParserTest | FAIL | 0 | PASS | 5.3s | 1.2s | 4.4x |
| core | org.keycloak.SkeletonKeyTokenTest | FAIL | 0 | PASS | 7.2s | 3.1s | 2.3x |
| core | org.keycloak.jose.JsonWebTokenTest | PASS | 9 | PASS | 2.5s | 0.9s | 2.8x |
| core | org.keycloak.jose.jwk.JWKUtilTest | FAIL | 5/6 (1 fail) | FAIL | 1.9s | 0.4s | 4.7x |
| core | org.keycloak.json.StringListMapDeserializerTest | PASS | 4 | PASS | 2.6s | 0.7s | 3.7x |
| core | org.keycloak.representations.IDTokenTest | FAIL | 0 | PASS | 1.7s | 0.9s | 1.9x |
| core | org.keycloak.representations.UserInfoTest | PASS | 1 | PASS | 2.4s | 0.8s | 3.0x |
| core | org.keycloak.representations.workflows.WorkflowDefinitionTest | PASS | 2 | PASS | 3.7s | 1.0s | 3.7x |
| core | org.keycloak.sdjwt.ArrayElementDisclosureTest | PASS | 2 | PASS | 3.4s | 0.8s | 4.2x |
| core | org.keycloak.sdjwt.ArrayElementSerializationTest | PASS | 1 | PASS | 1.9s | 0.7s | 2.7x |
| core | org.keycloak.sdjwt.ClaimVerifierTest | PASS | 4 | PASS | 1.9s | 0.5s | 3.8x |
| core | org.keycloak.sdjwt.DisclosureRedListTest | PASS | 7 | PASS | 1.9s | 0.7s | 2.7x |
| core | org.keycloak.sdjwt.IssuerSignedJWTTest | FAIL | 0 | PASS | 2.2s | 0.8s | 2.8x |
| core | org.keycloak.sdjwt.JsonClaimsetTest | PASS | 1 | PASS | 1.8s | 0.7s | 2.6x |
| core | org.keycloak.sdjwt.JsonNodeComparisonTest | PASS | 1 | PASS | 1.7s | 0.7s | 2.4x |
| core | org.keycloak.sdjwt.SdJWTSamplesTest | FAIL | 0 | PASS | 1.0s | 1.0s | 1.0x |
| core | org.keycloak.sdjwt.SdJwtTest | FAIL | 0 | PASS | 7.3s | 1.0s | 7.3x |
| core | org.keycloak.sdjwt.SdJwtUtilsTest | PASS | 6 | PASS | 1.7s | 0.7s | 2.4x |
| core | org.keycloak.sdjwt.TimeClaimVerifierTest | PASS | 15 | PASS | 2.0s | 0.7s | 2.9x |
| core | org.keycloak.sdjwt.UndisclosedClaimTest | FAIL | 0 | PASS | 0.2s | 0.8s | 0.2x |
| core | org.keycloak.sdjwt.consumer.SimplePresentationDefinitionTest | PASS | 3 | PASS | 2.1s | 0.8s | 2.6x |
| core | org.keycloak.sdjwt.sdjwtvp.KeyBindingJwtVerificationOptsTest | PASS | 3 | PASS | 1.8s | 0.4s | 4.5x |
| core | org.keycloak.util.BasicAuthHelperTest | PASS | 4 | PASS | 1.7s | 0.4s | 4.2x |
| core | org.keycloak.util.UriUtilsTest | PASS | 2 | PASS | 2.0s | 0.4s | 5.0x |
| crypto | org.keycloak.crypto.def.test.BCECDSACryptoProviderTest | FAIL | 0 | PASS | 1.5s | 1.8s | 0.8x |
| crypto | org.keycloak.crypto.def.test.BCEcdhEsAlgorithmProviderTest | FAIL | 0/2 (2 fail) | PASS | 3.6s | 2.4s | 1.5x |
| crypto | org.keycloak.crypto.def.test.CryptoPerfTest | PASS | 0 | PASS | 1.7s | 0.5s | 3.4x |
| crypto | org.keycloak.crypto.def.test.DefaultCertificateIdentityExtractorTest | FAIL | 4/5 (1 fail) | PASS | 3.6s | 1.6s | 2.2x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoAKPJWKTest | FAIL | 0 | PASS | 0.5s | 2.1s | 0.2x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoHmacTest | PASS | 2 | PASS | 4.2s | 2.0s | 2.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWETest | FAIL | 0 | PASS | 10.1s | 4.0s | 2.5x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKSUtilsTest | FAIL | 2/3 (1 fail) | PASS | 6.2s | 2.0s | 3.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKTest | FAIL | 3/10 (7 fail) | PASS | 14.8s | 3.6s | 4.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoKeyPairVerifierTest | FAIL | 0/4 (4 fail) | PASS | 3.2s | 2.0s | 1.6x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoRSAVerifierTest | FAIL | 8/9 (1 fail) | PASS | 20.2s | 3.6s | 5.6x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoUnitTest | PASS | 1 | PASS | 2.9s | 1.4s | 2.1x |
| crypto | org.keycloak.crypto.def.test.DefaultKeyStoreTypesTest | FAIL | 1/2 (1 fail) | PASS | 2.8s | 1.5s | 1.9x |
| crypto | org.keycloak.crypto.def.test.DefaultSecureRandomTest | PASS | 1 | PASS | 2.8s | 1.5s | 1.9x |
| crypto | org.keycloak.crypto.def.test.PemUtilsBCTest | FAIL | 3/6 (3 fail) | PASS | 7.1s | 2.0s | 3.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoJwtVcMetadataTrustedSdJwtIssuerTest | PASS | 16 | PASS | 22.8s | 2.2s | 10.4x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwsTest | PASS | 14 | PASS | 6.1s | 2.1s | 2.9x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtCreationAndSigningTest | PASS | 2 | PASS | 8.9s | 2.2s | 4.0x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtKeyBindingTest | FAIL | 2/7 (5 fail) | FAIL | 54.4s | 4.6s | 11.8x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtPresentationConsumerTest | PASS | 2 | PASS | 7.6s | 2.1s | 3.6x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPTest | PASS | 14 | PASS | 22.4s | 2.0s | 11.2x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPVerificationTest | CRASH | 0 | PASS | 11.6s | 2.6s | 4.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVerificationTest | PASS | 16 | PASS | 34.3s | 2.3s | 14.9x |

## Summary

- Concrete classes run: **49**
  - CRASH: 1
  - PASS: 27
  - FAIL: 21
- Tests executed (CratonVM): **188**, failures: **26**
- Total wall: CratonVM **318.1s** vs HotSpot **73.2s** (4.3x)
