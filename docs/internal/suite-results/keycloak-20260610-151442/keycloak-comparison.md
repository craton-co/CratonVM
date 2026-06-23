# Keycloak suite — CratonVM vs HotSpot

**Date:** 2026-06-10T18:30:29Z
**CratonVM:** `C:/craton/CratonVM/target/release/cratonvm.exe`  (HEAD 39aaffe3)
**HotSpot:** `C:/Program Files/Java/jdk-25/bin/java.exe` (java version 25.0.1 2025-10-21 LTS)
**Per-class timeout:** 240s   **Logs:** `C:/craton/CratonVM/test-infra/suite-results/keycloak-20260610-151442/`

| Module | Test class | CratonVM | tests | HotSpot | CV time | HS time | Slowdown |
|--------|-----------|----------|-------|---------|---------|---------|----------|
| core | org.keycloak.AtHashTest | PASS | 2 | PASS | 2.9s | 0.6s | 4.8x |
| core | org.keycloak.HashTest | PASS | 1 | PASS | 2.9s | 0.6s | 4.8x |
| core | org.keycloak.JsonParserTest | PASS | 10 | PASS | 14.5s | 1.4s | 10.4x |
| core | org.keycloak.SkeletonKeyTokenTest | PASS | 5 | PASS | 25.4s | 4.0s | 6.3x |
| core | org.keycloak.jose.JsonWebTokenTest | PASS | 9 | PASS | 4.7s | 1.1s | 4.3x |
| core | org.keycloak.jose.jwk.JWKUtilTest | FAIL | 5/6 (1 fail) | FAIL | 3.1s | 0.6s | 5.2x |
| core | org.keycloak.json.StringListMapDeserializerTest | PASS | 4 | PASS | 4.8s | 1.0s | 4.8x |
| core | org.keycloak.representations.IDTokenTest | PASS | 1 | PASS | 4.3s | 1.1s | 3.9x |
| core | org.keycloak.representations.UserInfoTest | PASS | 1 | PASS | 4.4s | 1.1s | 4.0x |
| core | org.keycloak.representations.workflows.WorkflowDefinitionTest | PASS | 2 | PASS | 6.5s | 1.2s | 5.4x |
| core | org.keycloak.sdjwt.ArrayElementDisclosureTest | PASS | 2 | PASS | 5.0s | 1.1s | 4.5x |
| core | org.keycloak.sdjwt.ArrayElementSerializationTest | PASS | 1 | PASS | 3.2s | 1.0s | 3.2x |
| core | org.keycloak.sdjwt.ClaimVerifierTest | PASS | 4 | PASS | 3.0s | 0.6s | 5.0x |
| core | org.keycloak.sdjwt.DisclosureRedListTest | PASS | 7 | PASS | 3.4s | 3.3s | 1.0x |
| core | org.keycloak.sdjwt.IssuerSignedJWTTest | PASS | 4 | PASS | 11.3s | 1.4s | 8.1x |
| core | org.keycloak.sdjwt.JsonClaimsetTest | PASS | 1 | PASS | 3.7s | 1.0s | 3.7x |
| core | org.keycloak.sdjwt.JsonNodeComparisonTest | PASS | 1 | PASS | 3.3s | 0.9s | 3.7x |
| core | org.keycloak.sdjwt.SdJWTSamplesTest | PASS | 4 | PASS | 5.8s | 1.2s | 4.8x |
| core | org.keycloak.sdjwt.SdJwtTest | PASS | 2 | PASS | 19.5s | 1.4s | 13.9x |
| core | org.keycloak.sdjwt.SdJwtUtilsTest | PASS | 6 | PASS | 3.4s | 0.9s | 3.8x |
| core | org.keycloak.sdjwt.TimeClaimVerifierTest | PASS | 15 | PASS | 3.8s | 1.0s | 3.8x |
| core | org.keycloak.sdjwt.UndisclosedClaimTest | PASS | 1 | PASS | 3.5s | 1.0s | 3.5x |
| core | org.keycloak.sdjwt.consumer.SimplePresentationDefinitionTest | PASS | 3 | PASS | 3.5s | 1.1s | 3.2x |
| core | org.keycloak.sdjwt.sdjwtvp.KeyBindingJwtVerificationOptsTest | PASS | 3 | PASS | 2.8s | 0.5s | 5.6x |
| core | org.keycloak.util.BasicAuthHelperTest | PASS | 4 | PASS | 2.8s | 0.6s | 4.7x |
| core | org.keycloak.util.UriUtilsTest | PASS | 2 | PASS | 2.9s | 0.5s | 5.8x |
| crypto | org.keycloak.crypto.def.test.BCECDSACryptoProviderTest | FAIL | 0/3 (3 fail) | PASS | 15.5s | 2.8s | 5.5x |
| crypto | org.keycloak.crypto.def.test.BCEcdhEsAlgorithmProviderTest | FAIL | 0/2 (2 fail) | PASS | 11.0s | 3.4s | 3.2x |
| crypto | org.keycloak.crypto.def.test.CryptoPerfTest | PASS | 0 | PASS | 3.4s | 0.6s | 5.7x |
| crypto | org.keycloak.crypto.def.test.DefaultCertificateIdentityExtractorTest | FAIL | 0/5 (5 fail) | PASS | 9.7s | 2.0s | 4.8x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoAKPJWKTest | FAIL | 0/6 (6 fail) | PASS | 10.3s | 2.7s | 3.8x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoHmacTest | PASS | 2 | PASS | 11.9s | 2.8s | 4.2x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWETest | FAIL | 1/11 (10 fail) | PASS | 23.7s | 6.4s | 3.7x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKSUtilsTest | FAIL | 2/3 (1 fail) | PASS | 22.2s | 2.6s | 8.5x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKTest | FAIL | 2/10 (8 fail) | PASS | 29.3s | 4.6s | 6.4x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoKeyPairVerifierTest | FAIL | 0/4 (4 fail) | PASS | 9.1s | 2.7s | 3.4x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoRSAVerifierTest | FAIL | 8/9 (1 fail) | PASS | 43.7s | 5.5s | 7.9x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoUnitTest | PASS | 1 | PASS | 7.5s | 2.0s | 3.8x |
| crypto | org.keycloak.crypto.def.test.DefaultKeyStoreTypesTest | FAIL | 1/2 (1 fail) | PASS | 8.0s | 2.0s | 4.0x |
| crypto | org.keycloak.crypto.def.test.DefaultSecureRandomTest | PASS | 1 | PASS | 7.8s | 2.0s | 3.9x |
| crypto | org.keycloak.crypto.def.test.PemUtilsBCTest | FAIL | 2/6 (4 fail) | PASS | 12.2s | 2.7s | 4.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoJwtVcMetadataTrustedSdJwtIssuerTest | PASS | 16 | PASS | 50.0s | 2.7s | 18.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwsTest | PASS | 14 | PASS | 15.3s | 2.7s | 5.7x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtCreationAndSigningTest | PASS | 2 | PASS | 15.6s | 2.9s | 5.4x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtKeyBindingTest | FAIL | 2/7 (5 fail) | FAIL | 65.0s | 5.5s | 11.8x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtPresentationConsumerTest | PASS | 2 | PASS | 19.6s | 3.0s | 6.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPTest | PASS | 14 | PASS | 52.1s | 2.9s | 18.0x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPVerificationTest | FAIL | 0 | PASS | 110.8s | 3.1s | 35.7x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVerificationTest | FAIL | 0 | PASS | 39.5s | 2.9s | 13.6x |

## Summary

- Concrete classes run: **49**
  - PASS: 34
  - FAIL: 15
- Tests executed (CratonVM): **221**, failures: **51**
- Total wall: CratonVM **747.6s** vs HotSpot **100.7s** (7.4x)
