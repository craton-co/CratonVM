# Keycloak full suite — CratonVM vs HotSpot

**Date:** 2026-06-14T12:23:35Z
**CratonVM:** `C:/craton/CratonVM-kcsuite/cratonvm-kc.exe`
**HotSpot:** `C:/Program Files/Java/jdk-25/bin/java.exe` (java version 25.0.1 2025-10-21 LTS)
**Per-class timeout:** 180s   **Logs:** `C:/craton/CratonVM/test-infra/suite-results/keycloak-iter8/`

| Module | Test class | CratonVM | tests | HotSpot | CV time | HS time | Slowdown |
|--------|-----------|----------|-------|---------|---------|---------|----------|
| common | org.keycloak.common.ProfileTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.crypto.CryptoIntegrationTest | FAIL | 0 | FAIL | 1.2s | 0.2s | 6.0x |
| common | org.keycloak.common.enums.SslRequiredTest | FAIL | 0 | FAIL | 1.2s | 0.2s | 6.0x |
| common | org.keycloak.common.util.Base64DecodeTest | FAIL | 0 | FAIL | 1.2s | 0.2s | 6.0x |
| common | org.keycloak.common.util.CollectionUtilTest | FAIL | 0 | FAIL | 1.2s | 0.2s | 6.0x |
| common | org.keycloak.common.util.HtmlUtilsTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.util.KeyUtilsTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.util.KeycloakUriBuilderTest | FAIL | 0 | FAIL | 1.2s | 0.2s | 6.0x |
| common | org.keycloak.common.util.KeystoreUtilTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.util.MultivaluedHashMapTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.util.PaddingUtilsTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.util.PathMatcherTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.util.StringPropertyReplacerTest | FAIL | 0 | FAIL | 1.3s | 0.2s | 6.5x |
| common | org.keycloak.common.util.StringSerializationTest | FAIL | 0 | FAIL | 1.2s | 0.2s | 6.0x |
| common | org.keycloak.common.util.URLEncodingTest | FAIL | 0 | FAIL | 1.2s | 0.2s | 6.0x |
| core | org.keycloak.AtHashTest | PASS | 2 | PASS | 1.4s | 0.5s | 2.8x |
| core | org.keycloak.HashTest | PASS | 1 | PASS | 1.6s | 0.5s | 3.2x |
| core | org.keycloak.JsonParserTest | PASS | 10 | PASS | 5.1s | 1.1s | 4.6x |
| core | org.keycloak.SkeletonKeyTokenTest | PASS | 5 | PASS | 16.2s | 2.6s | 6.2x |
| core | org.keycloak.jose.JsonWebTokenTest | PASS | 9 | PASS | 2.1s | 0.9s | 2.3x |
| core | org.keycloak.jose.jwk.JWKUtilTest | FAIL | 5/6 (1 fail) | FAIL | 1.7s | 0.5s | 3.4x |
| core | org.keycloak.json.StringListMapDeserializerTest | PASS | 4 | PASS | 2.1s | 0.8s | 2.6x |
| core | org.keycloak.representations.IDTokenTest | PASS | 1 | PASS | 2.0s | 0.8s | 2.5x |
| core | org.keycloak.representations.UserInfoTest | PASS | 1 | PASS | 2.0s | 0.9s | 2.2x |
| core | org.keycloak.representations.workflows.WorkflowDefinitionTest | PASS | 2 | PASS | 2.8s | 1.0s | 2.8x |
| core | org.keycloak.sdjwt.ArrayElementDisclosureTest | PASS | 2 | PASS | 2.6s | 0.9s | 2.9x |
| core | org.keycloak.sdjwt.ArrayElementSerializationTest | PASS | 1 | PASS | 2.0s | 0.7s | 2.9x |
| core | org.keycloak.sdjwt.ClaimVerifierTest | PASS | 4 | PASS | 2.0s | 0.5s | 4.0x |
| core | org.keycloak.sdjwt.DisclosureRedListTest | PASS | 7 | PASS | 2.0s | 0.7s | 2.9x |
| core | org.keycloak.sdjwt.IssuerSignedJWTTest | PASS | 4 | PASS | 2.2s | 0.9s | 2.4x |
| core | org.keycloak.sdjwt.JsonClaimsetTest | PASS | 1 | PASS | 1.6s | 0.7s | 2.3x |
| core | org.keycloak.sdjwt.JsonNodeComparisonTest | PASS | 1 | PASS | 1.8s | 0.8s | 2.2x |
| core | org.keycloak.sdjwt.SdJWTSamplesTest | PASS | 4 | PASS | 2.4s | 1.0s | 2.4x |
| core | org.keycloak.sdjwt.SdJwtTest | PASS | 2 | PASS | 2.8s | 1.1s | 2.5x |
| core | org.keycloak.sdjwt.SdJwtUtilsTest | PASS | 6 | PASS | 1.9s | 0.8s | 2.4x |
| core | org.keycloak.sdjwt.TimeClaimVerifierTest | PASS | 15 | PASS | 2.1s | 0.8s | 2.6x |
| core | org.keycloak.sdjwt.UndisclosedClaimTest | PASS | 1 | PASS | 1.8s | 0.8s | 2.2x |
| core | org.keycloak.sdjwt.consumer.SimplePresentationDefinitionTest | PASS | 3 | PASS | 3.0s | 0.9s | 3.3x |
| core | org.keycloak.sdjwt.sdjwtvp.KeyBindingJwtVerificationOptsTest | PASS | 3 | PASS | 1.8s | 0.5s | 3.6x |
| core | org.keycloak.util.BasicAuthHelperTest | PASS | 4 | PASS | 1.7s | 0.5s | 3.4x |
| core | org.keycloak.util.UriUtilsTest | PASS | 2 | PASS | 1.5s | 0.5s | 3.0x |
| crypto | org.keycloak.crypto.def.test.BCECDSACryptoProviderTest | FAIL | 0/3 (3 fail) | PASS | 5.4s | 1.7s | 3.2x |
| crypto | org.keycloak.crypto.def.test.BCEcdhEsAlgorithmProviderTest | FAIL | 0/2 (2 fail) | PASS | 4.8s | 2.7s | 1.8x |
| crypto | org.keycloak.crypto.def.test.CryptoPerfTest | PASS | 0 | PASS | 2.1s | 0.5s | 4.2x |
| crypto | org.keycloak.crypto.def.test.DefaultCertificateIdentityExtractorTest | PASS | 5 | PASS | 3.9s | 1.6s | 2.4x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoAKPJWKTest | PASS | 6 | PASS | 17.7s | 2.1s | 8.4x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoHmacTest | PASS | 2 | PASS | 4.5s | 2.1s | 2.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWETest | FAIL | 5/11 (6 fail) | PASS | 20.5s | 3.0s | 6.8x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKSUtilsTest | PASS | 3 | PASS | 4.3s | 1.4s | 3.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoJWKTest | FAIL | 8/10 (2 fail) | PASS | 17.3s | 2.5s | 6.9x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoKeyPairVerifierTest | PASS | 4 | PASS | 4.3s | 1.4s | 3.1x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoRSAVerifierTest | PASS | 9 | PASS | 9.8s | 2.6s | 3.8x |
| crypto | org.keycloak.crypto.def.test.DefaultCryptoUnitTest | PASS | 1 | PASS | 2.8s | 1.6s | 1.7x |
| crypto | org.keycloak.crypto.def.test.DefaultKeyStoreTypesTest | PASS | 2 | PASS | 3.2s | 1.0s | 3.2x |
| crypto | org.keycloak.crypto.def.test.DefaultSecureRandomTest | PASS | 1 | PASS | 2.3s | 1.1s | 2.1x |
| crypto | org.keycloak.crypto.def.test.PemUtilsBCTest | PASS | 6 | PASS | 6.4s | 1.4s | 4.6x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoJwtVcMetadataTrustedSdJwtIssuerTest | PASS | 16 | PASS | 6.1s | 1.4s | 4.4x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwsTest | PASS | 14 | PASS | 4.8s | 1.5s | 3.2x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtCreationAndSigningTest | PASS | 2 | PASS | 5.3s | 1.5s | 3.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtKeyBindingTest | FAIL | 4/7 (3 fail) | FAIL | 23.3s | 3.6s | 6.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtPresentationConsumerTest | PASS | 2 | PASS | 5.2s | 1.5s | 3.5x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPTest | PASS | 14 | PASS | 6.1s | 1.6s | 3.8x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVPVerificationTest | FAIL | 22/24 (2 fail) | PASS | 10.8s | 1.7s | 6.4x |
| crypto | org.keycloak.crypto.def.test.sdjwt.DefaultCryptoSdJwtVerificationTest | PASS | 16 | PASS | 9.0s | 1.7s | 5.3x |
| server-spi | org.keycloak.models.OtpPolicyTest | PASS | 2 | PASS | 1.6s | 0.3s | 5.3x |
| server-spi | org.keycloak.models.credential.CredentialModelTest | PASS | 5 | PASS | 3.5s | 0.7s | 5.0x |
| server-spi | org.keycloak.models.credential.RecoveryCodesUnitTest | PASS | 1 | PASS | 1.6s | 0.3s | 5.3x |
| server-spi | org.keycloak.provider.ProviderConfigurationBuilderTest | PASS | 2 | PASS | 1.6s | 0.4s | 4.0x |
| server-spi | org.keycloak.storage.StorageIdTest | PASS | 5 | PASS | 1.6s | 0.4s | 4.0x |
| server-spi | org.keycloak.utils.MapperTypeSerializerTest | PASS | 2 | PASS | 2.4s | 0.6s | 4.0x |
| server-spi | org.keycloak.utils.StringUtilTest | PASS | 1 | PASS | 1.6s | 0.4s | 4.0x |
| server-spi-private | org.keycloak.broker.provider.util.IdentityBrokerStateTest | PASS | 6 | PASS | 2.4s | 0.8s | 3.0x |
| server-spi-private | org.keycloak.component.ComponentModelScopeTest | PASS | 1 | PASS | 1.7s | 0.4s | 4.2x |
| server-spi-private | org.keycloak.connections.httpclient.RetryConfigTest | PASS | 14 | PASS | 1.7s | 0.4s | 4.2x |
| server-spi-private | org.keycloak.connections.jpa.support.EntityManagerProxyTest | PASS | 1 | PASS | 1.7s | 0.4s | 4.2x |
| server-spi-private | org.keycloak.http.simple.SimpleHttpTest | FAIL | 0/1 (1 fail) | FAIL | 1.5s | 0.3s | 5.0x |
| server-spi-private | org.keycloak.http.simple.SimpleHttpTest$RequestConsideringEncodingTest | PASS | 10 | PASS | 2.6s | 0.7s | 3.7x |
| server-spi-private | org.keycloak.http.simple.SimpleHttpTest$ResponseConsideringCharsetTest | PASS | 4 | PASS | 2.0s | 0.6s | 3.3x |
| server-spi-private | org.keycloak.models.BrowserSecurityHeadersTest | PASS | 3 | PASS | 1.9s | 0.4s | 4.7x |
| server-spi-private | org.keycloak.models.CredentialModelBackwardsCompatibilityTest | PASS | 4 | PASS | 3.0s | 0.8s | 3.8x |
| server-spi-private | org.keycloak.models.HmacTest | PASS | 1 | PASS | 1.7s | 0.4s | 4.2x |
| server-spi-private | org.keycloak.models.KeycloakModelUtilsTest | PASS | 9 | PASS | 1.8s | 0.5s | 3.6x |
| server-spi-private | org.keycloak.models.KeycloakModelUtilsTest$GroupAdapterTest | FAIL | 0/1 (1 fail) | FAIL | 1.7s | 0.4s | 4.2x |
| server-spi-private | org.keycloak.models.KeycloakModelUtilsTest$OrganizationModelTest | FAIL | 0/1 (1 fail) | FAIL | 1.7s | 0.3s | 5.7x |
| server-spi-private | org.keycloak.models.ModelVersionTest | PASS | 1 | PASS | 1.5s | 0.5s | 3.0x |
| server-spi-private | org.keycloak.models.TotpTest | PASS | 4 | PASS | 2.2s | 0.8s | 2.8x |
| server-spi-private | org.keycloak.models.utils.KeycloakModelUtilsTest | PASS | 2 | PASS | 1.7s | 0.4s | 4.2x |
| server-spi-private | org.keycloak.models.utils.SessionExpirationUtilsTest | PASS | 8 | PASS | 1.7s | 0.5s | 3.4x |
| server-spi-private | org.keycloak.models.utils.StripSecretsUtilsTest | PASS | 9 | PASS | 1.8s | 0.4s | 4.5x |
| server-spi-private | org.keycloak.policy.DenylistPasswordPolicyProviderTest | FAIL | 3/10 (7 fail) | FAIL | 2.0s | 0.6s | 3.3x |
| server-spi-private | org.keycloak.policy.NotEmailPasswordPolicyProviderTest | PASS | 3 | PASS | 1.8s | 0.4s | 4.5x |
| server-spi-private | org.keycloak.utils.StreamsUtilTest | FAIL | 0/8 (8 fail) | PASS | 1.8s | 0.4s | 4.5x |

## Summary

- Concrete classes run: **92**
  - PASS: 65
  - FAIL: 27
- Tests executed (CratonVM): **380**, failures: **37**
- Total wall: CratonVM **322.7s** vs HotSpot **79.4s** (4.1x)
