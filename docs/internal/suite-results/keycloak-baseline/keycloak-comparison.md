# Keycloak full suite — CratonVM vs HotSpot

**Date:** 2026-06-14T02:14:31Z
**CratonVM:** `C:/craton/CratonVM/target/release/cratonvm.exe`
**HotSpot:** `C:/Program Files/Java/jdk-25/bin/java.exe` (java version 25.0.1 2025-10-21 LTS)
**Per-class timeout:** 180s   **Logs:** `C:/craton/CratonVM/test-infra/suite-results/keycloak-baseline/`

| Module | Test class | CratonVM | tests | HotSpot | CV time | HS time | Slowdown |
|--------|-----------|----------|-------|---------|---------|---------|----------|
| common | org.keycloak.common.ProfileTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| common | org.keycloak.common.crypto.CryptoIntegrationTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| common | org.keycloak.common.enums.SslRequiredTest | FAIL | 0 | FAIL | 1.9s | 0.3s | 6.3x |
| common | org.keycloak.common.util.Base64DecodeTest | FAIL | 0 | FAIL | 2.0s | 0.3s | 6.7x |
| common | org.keycloak.common.util.CollectionUtilTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| common | org.keycloak.common.util.HtmlUtilsTest | FAIL | 0 | FAIL | 1.9s | 0.3s | 6.3x |
| common | org.keycloak.common.util.KeyUtilsTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| common | org.keycloak.common.util.KeycloakUriBuilderTest | FAIL | 0 | FAIL | 1.9s | 0.3s | 6.3x |
| common | org.keycloak.common.util.KeystoreUtilTest | FAIL | 0 | FAIL | 1.9s | 0.3s | 6.3x |
| common | org.keycloak.common.util.MultivaluedHashMapTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| common | org.keycloak.common.util.PaddingUtilsTest | FAIL | 0 | FAIL | 2.0s | 0.3s | 6.7x |
| common | org.keycloak.common.util.PathMatcherTest | FAIL | 0 | FAIL | 1.9s | 0.2s | 9.5x |
| common | org.keycloak.common.util.StringPropertyReplacerTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| common | org.keycloak.common.util.StringSerializationTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| common | org.keycloak.common.util.URLEncodingTest | FAIL | 0 | FAIL | 1.8s | 0.3s | 6.0x |
| core | org.keycloak.AtHashTest | PASS | 2 | PASS | 4.5s | 1.5s | 3.0x |
| core | org.keycloak.HashTest | PASS | 1 | PASS | 4.9s | 1.1s | 4.5x |
| core | org.keycloak.JsonParserTest | PASS | 10 | PASS | 12.5s | 1.1s | 11.4x |
| core | org.keycloak.SkeletonKeyTokenTest | FAIL | 0 | PASS | 10.0s | 2.1s | 4.8x |
| core | org.keycloak.jose.JsonWebTokenTest | PASS | 9 | PASS | 3.6s | 1.2s | 3.0x |
| core | org.keycloak.jose.jwk.JWKUtilTest | FAIL | 5/6 (1 fail) | FAIL | 2.4s | 0.5s | 4.8x |
| core | org.keycloak.json.StringListMapDeserializerTest | PASS | 4 | PASS | 3.0s | 0.8s | 3.8x |
| core | org.keycloak.representations.IDTokenTest | FAIL | 0 | PASS | 1.8s | 1.0s | 1.8x |
| core | org.keycloak.representations.UserInfoTest | PASS | 1 | PASS | 3.3s | 1.0s | 3.3x |
| core | org.keycloak.representations.workflows.WorkflowDefinitionTest | PASS | 2 | PASS | 4.4s | 1.1s | 4.0x |
| core | org.keycloak.sdjwt.ArrayElementDisclosureTest | PASS | 2 | PASS | 3.6s | 0.9s | 4.0x |
| core | org.keycloak.sdjwt.ArrayElementSerializationTest | PASS | 1 | PASS | 2.8s | 0.8s | 3.5x |
| core | org.keycloak.sdjwt.ClaimVerifierTest | PASS | 4 | PASS | 2.5s | 0.5s | 5.0x |
| core | org.keycloak.sdjwt.DisclosureRedListTest | PASS | 7 | PASS | 2.8s | 0.7s | 4.0x |
| core | org.keycloak.sdjwt.IssuerSignedJWTTest | PASS | 4 | PASS | 3.3s | 0.9s | 3.7x |
| core | org.keycloak.sdjwt.JsonClaimsetTest | PASS | 1 | PASS | 2.7s | 0.8s | 3.4x |
| core | org.keycloak.sdjwt.JsonNodeComparisonTest | PASS | 1 | PASS | 2.7s | 0.8s | 3.4x |
| core | org.keycloak.sdjwt.SdJWTSamplesTest | PASS | 4 | PASS | 3.5s | 1.0s | 3.5x |
| core | org.keycloak.sdjwt.SdJwtTest | PASS | 2 | PASS | 4.7s | 1.2s | 3.9x |
| core | org.keycloak.sdjwt.SdJwtUtilsTest | PASS | 6 | PASS | 2.9s | 0.7s | 4.1x |
| core | org.keycloak.sdjwt.TimeClaimVerifierTest | PASS | 15 | PASS | 3.0s | 0.8s | 3.8x |
| core | org.keycloak.sdjwt.UndisclosedClaimTest | PASS | 1 | PASS | 2.7s | 0.9s | 3.0x |
| core | org.keycloak.sdjwt.consumer.SimplePresentationDefinitionTest | PASS | 3 | PASS | 3.0s | 0.8s | 3.8x |
| core | org.keycloak.sdjwt.sdjwtvp.KeyBindingJwtVerificationOptsTest | PASS | 3 | PASS | 2.6s | 0.5s | 5.2x |
| core | org.keycloak.util.BasicAuthHelperTest | PASS | 4 | PASS | 2.6s | 0.4s | 6.5x |
| core | org.keycloak.util.UriUtilsTest | PASS | 2 | PASS | 2.5s | 0.5s | 5.0x |
| crypto | org.keycloak.crypto.def.test.BCECDSACryptoProviderTest | FAIL | 0 | PASS | 2.8s | 1.7s | 1.6x |
| crypto | org.keycloak.crypto.def.test.BCEcdhEsAlgorithmProviderTest | FAIL | 0/2 (2 fail) | PASS | 7.2s | 2.9s | 2.5x |
| crypto | org.keycloak.crypto.def.test.CryptoPerfTest | PASS | 0 | PASS | 2.7s | 0.5s | 5.4x |
| crypto | org.keycloak.crypto.def.test.DefaultCertificateIdentityExtractorTest | FAIL | 3/5 (2 fail) | PASS | 5.9s | 1.6s | 3.7x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoAKPJWKTest | PASS | 6 | PASS | 27.1s | 2.1s | 12.9x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoHmacTest | PASS | 2 | PASS | 5.9s | 2.1s | 2.8x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWETest | FAIL | 5/11 (6 fail) | PASS | 23.6s | 4.6s | 5.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKSUtilsTest | FAIL | 0 | PASS | 1.8s | 2.1s | 0.9x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKTest | FAIL | 2/10 (8 fail) | PASS | 27.5s | 4.0s | 6.9x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoKeyPairVerifierTest | FAIL | 0/4 (4 fail) | PASS | 7.0s | 2.1s | 3.3x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoRSAVerifierTest | FAIL | 8/9 (1 fail) | PASS | 20.7s | 5.1s | 4.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoUnitTest | PASS | 1 | PASS | 4.3s | 1.5s | 2.9x |
| crypto | org.keycloak.crypto.def.test.DefaultKeyStoreTypesTest | PASS | 2 | PASS | 4.9s | 1.7s | 2.9x |
| crypto | org.keycloak.crypto.def.test.DefaultSecureRandomTest | PASS | 1 | PASS | 4.1s | 1.6s | 2.6x |
| crypto | org.keycloak.crypto.def.test.PemUtilsBCTest | FAIL | 4/6 (2 fail) | PASS | 8.8s | 2.7s | 3.3x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoJwtVcMetadataTrustedSdJwtIssuerTest | PASS | 16 | PASS | 11.4s | 2.1s | 5.4x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwsTest | PASS | 14 | PASS | 8.4s | 2.1s | 4.0x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtCreationAndSigningTest | PASS | 2 | PASS | 8.8s | 2.2s | 4.0x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtKeyBindingTest | FAIL | 2/7 (5 fail) | FAIL | 55.3s | 11.5s | 4.8x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtPresentationConsumerTest | PASS | 2 | PASS | 9.3s | 2.3s | 4.0x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPTest | PASS | 14 | PASS | 10.9s | 2.3s | 4.7x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPVerificationTest | FAIL | 22/24 (2 fail) | PASS | 18.5s | 2.6s | 7.1x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVerificationTest | FAIL | 0 | PASS | 13.7s | 2.4s | 5.7x |
| server-spi | org.keycloak.models.OtpPolicyTest | PASS | 2 | PASS | 2.6s | 0.5s | 5.2x |
| server-spi | org.keycloak.models.credential.CredentialModelTest | FAIL | 1/5 (4 fail) | PASS | 3.5s | 1.1s | 3.2x |
| server-spi | org.keycloak.models.credential.RecoveryCodesUnitTest | PASS | 1 | PASS | 2.5s | 0.6s | 4.2x |
| server-spi | org.keycloak.provider.ProviderConfigurationBuilderTest | PASS | 2 | PASS | 2.6s | 0.8s | 3.2x |
| server-spi | org.keycloak.storage.StorageIdTest | PASS | 5 | PASS | 3.5s | 0.8s | 4.4x |
| server-spi | org.keycloak.utils.MapperTypeSerializerTest | PASS | 2 | PASS | 5.1s | 1.8s | 2.8x |
| server-spi | org.keycloak.utils.StringUtilTest | PASS | 1 | PASS | 2.4s | 1.0s | 2.4x |
| server-spi-private | org.keycloak.broker.provider.util.IdentityBrokerStateTest | FAIL | 0 | PASS | 1.8s | 1.2s | 1.5x |
| server-spi-private | org.keycloak.component.ComponentModelScopeTest | PASS | 1 | PASS | 2.9s | 0.6s | 4.8x |
| server-spi-private | org.keycloak.connections.httpclient.RetryConfigTest | PASS | 14 | PASS | 3.0s | 0.7s | 4.3x |
| server-spi-private | org.keycloak.connections.jpa.support.EntityManagerProxyTest | PASS | 1 | PASS | 2.8s | 0.6s | 4.7x |
| server-spi-private | org.keycloak.http.simple.SimpleHttpTest | FAIL | 0/1 (1 fail) | FAIL | 2.7s | 0.6s | 4.5x |
| server-spi-private | org.keycloak.http.simple.SimpleHttpTest$RequestConsideringEncodingTest | PASS | 10 | PASS | 4.4s | 1.3s | 3.4x |
| server-spi-private | org.keycloak.http.simple.SimpleHttpTest$ResponseConsideringCharsetTest | PASS | 4 | PASS | 3.4s | 0.9s | 3.8x |
| server-spi-private | org.keycloak.models.BrowserSecurityHeadersTest | PASS | 3 | PASS | 2.8s | 0.6s | 4.7x |
| server-spi-private | org.keycloak.models.CredentialModelBackwardsCompatibilityTest | FAIL | 3/4 (1 fail) | PASS | 5.0s | 1.3s | 3.8x |
| server-spi-private | org.keycloak.models.HmacTest | PASS | 1 | PASS | 2.7s | 0.6s | 4.5x |
| server-spi-private | org.keycloak.models.KeycloakModelUtilsTest | PASS | 9 | PASS | 2.9s | 0.6s | 4.8x |
| server-spi-private | org.keycloak.models.KeycloakModelUtilsTest$GroupAdapterTest | FAIL | 0/1 (1 fail) | FAIL | 2.7s | 0.7s | 3.9x |
| server-spi-private | org.keycloak.models.KeycloakModelUtilsTest$OrganizationModelTest | FAIL | 0/1 (1 fail) | FAIL | 2.7s | 0.6s | 4.5x |
| server-spi-private | org.keycloak.models.ModelVersionTest | PASS | 1 | PASS | 2.5s | 0.6s | 4.2x |
| server-spi-private | org.keycloak.models.TotpTest | FAIL | 0 | PASS | 3.6s | 1.2s | 3.0x |
| server-spi-private | org.keycloak.models.utils.KeycloakModelUtilsTest | PASS | 2 | PASS | 2.8s | 0.7s | 4.0x |
| server-spi-private | org.keycloak.models.utils.SessionExpirationUtilsTest | FAIL | 0 | PASS | 0.3s | 0.8s | 0.4x |
| server-spi-private | org.keycloak.models.utils.StripSecretsUtilsTest | FAIL | 7/9 (2 fail) | PASS | 3.0s | 0.7s | 4.3x |
| server-spi-private | org.keycloak.policy.DenylistPasswordPolicyProviderTest | FAIL | 3/10 (7 fail) | FAIL | 3.3s | 1.0s | 3.3x |
| server-spi-private | org.keycloak.policy.NotEmailPasswordPolicyProviderTest | PASS | 3 | PASS | 2.8s | 0.6s | 4.7x |
| server-spi-private | org.keycloak.utils.StreamsUtilTest | FAIL | 0/8 (8 fail) | PASS | 2.8s | 0.6s | 4.7x |

## Summary

- Concrete classes run: **92**
  - PASS: 51
  - FAIL: 41
- Tests executed (CratonVM): **334**, failures: **58**
- Total wall: CratonVM **499.9s** vs HotSpot **114.9s** (4.4x)
