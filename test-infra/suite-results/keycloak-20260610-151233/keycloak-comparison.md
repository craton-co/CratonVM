# Keycloak suite — CratonVM vs HotSpot

**Date:** 2026-06-10T18:30:31Z
**CratonVM:** `C:/craton/CratonVM/target/release/cratonvm.exe`  (HEAD 39aaffe3)
**HotSpot:** `C:/Program Files/Java/jdk-25/bin/java.exe` (java version 25.0.1 2025-10-21 LTS)
**Per-class timeout:** 240s   **Logs:** `C:/craton/CratonVM/test-infra/suite-results/keycloak-20260610-151233/`

| Module | Test class | CratonVM | tests | HotSpot | CV time | HS time | Slowdown |
|--------|-----------|----------|-------|---------|---------|---------|----------|
| core | org.keycloak.AtHashTest | PASS | 2 | PASS | 1.8s | 0.4s | 4.5x |
| core | org.keycloak.HashTest | PASS | 1 | PASS | 1.7s | 0.3s | 5.7x |
| core | org.keycloak.JsonParserTest | PASS | 10 | PASS | 7.5s | 0.8s | 9.4x |
| core | org.keycloak.KeyPairVerifierTest | FAIL | -1/0 (1 fail) | FAIL | 1.8s | 0.4s | 4.5x |
| core | org.keycloak.RSAVerifierTest | FAIL | -1/0 (1 fail) | FAIL | 2.0s | 0.4s | 5.0x |
| core | org.keycloak.SkeletonKeyTokenTest | PASS | 5 | PASS | 17.7s | 3.6s | 4.9x |
| core | org.keycloak.authentication.x509.CertificateIdentityExtractorTest | FAIL | -1/0 (1 fail) | FAIL | 2.1s | 0.4s | 5.2x |
| core | org.keycloak.jose.HmacTest | FAIL | -1/0 (1 fail) | FAIL | 1.7s | 0.3s | 5.7x |
| core | org.keycloak.jose.JWETest | FAIL | -1/0 (1 fail) | FAIL | 1.6s | 0.3s | 5.3x |
| core | org.keycloak.jose.JsonWebTokenTest | PASS | 9 | PASS | 2.3s | 0.6s | 3.8x |
| core | org.keycloak.jose.jwk.AKPJWKTest | FAIL | -1/0 (1 fail) | FAIL | 1.4s | 0.4s | 3.5x |
| core | org.keycloak.jose.jwk.JWKTest | FAIL | -1/0 (1 fail) | FAIL | 1.6s | 0.4s | 4.0x |
| core | org.keycloak.jose.jwk.JWKUtilTest | FAIL | 5/6 (1 fail) | FAIL | 1.8s | 0.5s | 3.6x |
| core | org.keycloak.json.StringListMapDeserializerTest | PASS | 4 | PASS | 4.6s | 0.9s | 5.1x |
| core | org.keycloak.representations.IDTokenTest | PASS | 1 | PASS | 4.2s | 1.1s | 3.8x |
| core | org.keycloak.representations.UserInfoTest | PASS | 1 | PASS | 4.2s | 1.1s | 3.8x |
| core | org.keycloak.representations.workflows.WorkflowDefinitionTest | PASS | 2 | PASS | 6.3s | 1.1s | 5.7x |
| core | org.keycloak.sdjwt.ArrayElementDisclosureTest | PASS | 2 | PASS | 5.2s | 1.2s | 4.3x |
| core | org.keycloak.sdjwt.ArrayElementSerializationTest | PASS | 1 | PASS | 3.3s | 1.0s | 3.3x |
| core | org.keycloak.sdjwt.ClaimVerifierTest | PASS | 4 | PASS | 3.3s | 0.8s | 4.1x |
| core | org.keycloak.sdjwt.DisclosureRedListTest | PASS | 7 | PASS | 3.6s | 1.2s | 3.0x |
| core | org.keycloak.sdjwt.IssuerSignedJWTTest | PASS | 4 | PASS | 5.6s | 1.3s | 4.3x |
| core | org.keycloak.sdjwt.JsonClaimsetTest | PASS | 1 | PASS | 3.6s | 1.2s | 3.0x |
| core | org.keycloak.sdjwt.JsonNodeComparisonTest | PASS | 1 | PASS | 3.6s | 1.1s | 3.3x |
| core | org.keycloak.sdjwt.SdJWTSamplesTest | PASS | 4 | PASS | 5.6s | 1.4s | 4.0x |
| core | org.keycloak.sdjwt.SdJwsTest | FAIL | -1/0 (1 fail) | FAIL | 5.2s | 1.4s | 3.7x |
| core | org.keycloak.sdjwt.SdJwtCreationAndSigningTest | FAIL | -1/0 (1 fail) | FAIL | 3.1s | 0.8s | 3.9x |
| core | org.keycloak.sdjwt.SdJwtTest | PASS | 2 | PASS | 19.8s | 1.5s | 13.2x |
| core | org.keycloak.sdjwt.SdJwtUtilsTest | PASS | 6 | PASS | 3.5s | 1.1s | 3.2x |
| core | org.keycloak.sdjwt.SdJwtVerificationTest | FAIL | -1/0 (1 fail) | FAIL | 5.1s | 1.4s | 3.6x |
| core | org.keycloak.sdjwt.TimeClaimVerifierTest | PASS | 15 | PASS | 3.9s | 1.1s | 3.5x |
| core | org.keycloak.sdjwt.UndisclosedClaimTest | PASS | 1 | PASS | 3.4s | 1.1s | 3.1x |
| core | org.keycloak.sdjwt.consumer.JwtVcMetadataTrustedSdJwtIssuerTest | FAIL | -1/0 (1 fail) | FAIL | 3.2s | 0.8s | 4.0x |
| core | org.keycloak.sdjwt.consumer.SdJwtPresentationConsumerTest | FAIL | -1/0 (1 fail) | FAIL | 12.0s | 2.5s | 4.8x |
| core | org.keycloak.sdjwt.consumer.SimplePresentationDefinitionTest | PASS | 3 | PASS | 4.3s | 1.2s | 3.6x |
| core | org.keycloak.sdjwt.sdjwtvp.KeyBindingJwtVerificationOptsTest | PASS | 3 | PASS | 3.1s | 0.8s | 3.9x |
| core | org.keycloak.sdjwt.sdjwtvp.SdJwtKeyBindingTest | FAIL | -1/0 (1 fail) | FAIL | 3.2s | 0.8s | 4.0x |
| core | org.keycloak.sdjwt.sdjwtvp.SdJwtVPTest | FAIL | -1/0 (1 fail) | FAIL | 3.2s | 0.8s | 4.0x |
| core | org.keycloak.sdjwt.sdjwtvp.SdJwtVPVerificationTest | FAIL | -1/0 (1 fail) | FAIL | 5.1s | 1.4s | 3.6x |
| core | org.keycloak.util.BasicAuthHelperTest | PASS | 4 | PASS | 3.2s | 0.7s | 4.6x |
| core | org.keycloak.util.JWKSUtilsTest | FAIL | -1/0 (1 fail) | FAIL | 3.2s | 0.8s | 4.0x |
| core | org.keycloak.util.PemUtilsTest | FAIL | -1/0 (1 fail) | FAIL | 3.3s | 0.8s | 4.1x |
| core | org.keycloak.util.UriUtilsTest | PASS | 2 | PASS | 3.1s | 0.8s | 3.9x |
| crypto | org.keycloak.crypto.def.test.BCECDSACryptoProviderTest | FAIL | 0/3 (3 fail) | PASS | 15.7s | 2.2s | 7.1x |
| crypto | org.keycloak.crypto.def.test.BCEcdhEsAlgorithmProviderTest | FAIL | 0/2 (2 fail) | PASS | 11.0s | 4.1s | 2.7x |
| crypto | org.keycloak.crypto.def.test.CryptoPerfTest | PASS | 0 | PASS | 3.7s | 0.8s | 4.6x |
| crypto | org.keycloak.crypto.def.test.DefaultCertificateIdentityExtractorTest | FAIL | 0/5 (5 fail) | PASS | 10.4s | 2.2s | 4.7x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoAKPJWKTest | FAIL | 0/6 (6 fail) | PASS | 10.3s | 2.8s | 3.7x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoHmacTest | PASS | 2 | PASS | 11.7s | 2.9s | 4.0x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWETest | FAIL | 1/11 (10 fail) | PASS | 42.4s | 8.3s | 5.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKSUtilsTest | FAIL | 2/3 (1 fail) | PASS | 16.1s | 3.0s | 5.4x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKTest | FAIL | 2/10 (8 fail) | PASS | 24.4s | 5.0s | 4.9x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoKeyPairVerifierTest | FAIL | 0/4 (4 fail) | PASS | 9.0s | 2.8s | 3.2x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoRSAVerifierTest | FAIL | 8/9 (1 fail) | PASS | 39.6s | 5.5s | 7.2x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoUnitTest | PASS | 1 | PASS | 7.9s | 2.1s | 3.8x |
| crypto | org.keycloak.crypto.def.test.DefaultKeyStoreTypesTest | FAIL | 1/2 (1 fail) | PASS | 7.9s | 2.1s | 3.8x |
| crypto | org.keycloak.crypto.def.test.DefaultSecureRandomTest | PASS | 1 | PASS | 8.0s | 2.3s | 3.5x |
| crypto | org.keycloak.crypto.def.test.PemUtilsBCTest | FAIL | 2/6 (4 fail) | PASS | 12.4s | 3.6s | 3.4x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoJwtVcMetadataTrustedSdJwtIssuerTest | PASS | 16 | PASS | 49.6s | 2.9s | 17.1x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwsTest | PASS | 14 | PASS | 15.7s | 3.0s | 5.2x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtCreationAndSigningTest | PASS | 2 | PASS | 18.2s | 3.0s | 6.1x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtKeyBindingTest | FAIL | 2/7 (5 fail) | FAIL | 54.9s | 5.9s | 9.3x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtPresentationConsumerTest | PASS | 2 | PASS | 18.5s | 3.1s | 6.0x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPTest | PASS | 14 | PASS | 51.6s | 3.0s | 17.2x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPVerificationTest | FAIL | 0 | PASS | 108.2s | 3.2s | 33.8x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVerificationTest | FAIL | 0 | PASS | 9.7s | 3.1s | 3.1x |

## Summary

- Concrete classes run: **66**
  - PASS: 34
  - FAIL: 32
- Tests executed (CratonVM): **221**, failures: **68**
- Total wall: CratonVM **745.9s** vs HotSpot **118.9s** (6.3x)
