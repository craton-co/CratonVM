# X.509 certificate: extracting the Subject DN's `CN` (Common Name) attribute returns `null` for a real, externally-loaded certificate

Status: open — not yet root-caused in CratonVM source, but cleanly reproducible with a concrete expected value

Date observed: 2026-07-10/11 (both the initial investigation pass and the refresh rerun; unaffected by the
recent batch of fixes)

## Summary

`crypto/elytron :: ElytronCertificateIdentityExtractorTest::testX509SubjectCommonName` consistently fails:

```
=> java.lang.AssertionError: expected:<899700252580> but was:<null>
   org.keycloak.authentication.x509.CertificateIdentityExtractorTest.testX509SubjectCommonName(CertificateIdentityExtractorTest.java:116)
```

Test source (`apps/keycloak/core/src/test/java/org/keycloak/authentication/x509/CertificateIdentityExtractorTest.java`,
lines 105-117):

```java
private static final Function<X509Certificate[], Principal> subject = certs -> {
    return certs[0].getSubjectX500Principal();
};

@Test
public void testX509SubjectCommonName() throws Exception {
    UserIdentityExtractor extractor = CryptoIntegration.getProvider().getIdentityExtractorProvider().getX500NameExtractor("CN", subject);
    X509Certificate cert = getCertificate(ANS_CERT_PATH);
    Object cn = extractor.extractUserIdentity(new X509Certificate[] { cert });
    Assert.assertEquals("899700252580", cn);
}
```

This decodes a **real, hardcoded PEM certificate fixture** (`ANS_CERT_PATH`, loaded via `PemUtils.decodeCertificate`)
whose Subject DN's `CN` attribute is expected to be the literal string `"899700252580"` (an account/serial-style
identifier baked into the test fixture certificate), then asks CratonVM's crypto provider to extract that specific
RDN (Relative Distinguished Name) attribute via `getX500NameExtractor("CN", ...)`. Instead of the expected value,
extraction returns `null`.

## Notes

- This is **not** the already-fixed X.509 `AuthorityKeyIdentifier`/`getEncoded()` bug from the earlier
  investigation (that was about certificates *generated* by CratonVM having malformed DER encoding) — this test
  loads a real, externally-authored PEM certificate and only reads its Subject DN, no certificate generation
  involved.
- Not yet determined whether this is a genuine RDN-extraction gap (CratonVM's `X500Principal`/`X500Name` handling
  not finding the `CN` attribute type in this certificate's Subject DN for some reason specific to this
  certificate's encoding), or something narrower to `getX500NameExtractor`'s specific implementation.

## Next steps

1. Find `CryptoProvider.getIdentityExtractorProvider().getX500NameExtractor("CN", ...)`'s implementation and see
   exactly how it parses the `X500Principal`/`X500Name` to extract a named RDN attribute.
2. Get the actual PEM certificate fixture (`ANS_CERT_PATH` in the test) and manually inspect its Subject DN
   structure (e.g. via `openssl x509 -text` or `javap`/direct ASN.1 dump) to confirm the CN attribute is present
   and where, then trace why CratonVM's extraction path returns null for it specifically.
3. Check whether a simpler, minimal repro (a synthetic cert with just a `CN=test` subject, built via
   `X500NameBuilder` at test time rather than an external fixture) also fails — this would rule out anything
   fixture-specific.

## Repro

```
cd C:\craton\CratonVM-keycloak-nonpassed-v2-20260710
$jdk = '"C:\Program Files\Java\jdk-25"'
powershell -NoProfile -ExecutionPolicy Bypass -File apps\keycloak-suite-runner\run-keycloak-suite.ps1 -Vm craton -Jit on -TimeoutSec 60 -Parallel 1 -RunName repro-x509-cn-extraction -ClassList <(printf 'module\tclass\ncrypto/elytron\torg.keycloak.crypto.elytron.test.ElytronCertificateIdentityExtractorTest\n') -KeycloakRoot apps\keycloak -Exe target\release\cratonvm-nonpassed-v2-refresh-20260711.exe -JdkHome $jdk
```

## Evidence

`C:\craton\CratonVM-keycloak-nonpassed-v2-20260710\apps\keycloak-suite-runner\.suite\results\nonpassed-before-refresh-shard1\all-jit\logs\crypto_elytron.org.keycloak.crypto.elytron.test.ElytronCertificateIdentityExtractorTest.out.log`,
consistent across both the initial investigation pass (2026-07-10) and the refresh rerun (2026-07-11). Source:
`apps/keycloak/core/src/test/java/org/keycloak/authentication/x509/CertificateIdentityExtractorTest.java` (line 105-117).
