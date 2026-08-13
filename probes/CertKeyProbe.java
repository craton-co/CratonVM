import io.netty.pkitesting.CertificateBuilder;
import io.netty.pkitesting.X509Bundle;

import java.security.PublicKey;
import java.security.cert.X509Certificate;
import java.time.Instant;
import java.time.temporal.ChronoUnit;

/**
 * `CertificateBuilder.buildIssuedBy` calls
 * `preferredSignatureAlgorithm(issuerBundle.getCertificate().getPublicKey())`,
 * and that call raised
 * `NoSuchMethodError: sun.security.x509.X509CertImpl.getAlgorithm()`.
 * `getAlgorithm()` is `java.security.Key`'s, so the receiver was a CERTIFICATE:
 * something returned the cert where a key belongs. Print what each step
 * actually hands back.
 */
public final class CertKeyProbe {
    static void p(String k, Object v) { System.out.println(k + "=" + v); }

    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }

    public static void main(String[] a) throws Exception {
        Instant now = Instant.now();
        CertificateBuilder base = new CertificateBuilder()
                .subject("CN=probe.netty.io, O=Netty")
                .notBefore(now.minus(1, ChronoUnit.DAYS))
                .notAfter(now.plus(1, ChronoUnit.DAYS));

        X509Bundle root = base.copy().ecp256()
                .setKeyUsage(true, CertificateBuilder.KeyUsage.digitalSignature,
                             CertificateBuilder.KeyUsage.keyCertSign)
                .setIsCertificateAuthority(true)
                .buildSelfSigned();
        p("root.bundleCls", cls(root));

        X509Certificate cert = root.getCertificate();
        p("root.cert.cls", cls(cert));
        p("root.cert.sigAlgName", cert.getSigAlgName());

        PublicKey fromCert = cert.getPublicKey();
        p("cert.getPublicKey.cls", cls(fromCert));
        p("cert.getPublicKey.isCertificate", fromCert instanceof java.security.cert.Certificate);
        try { p("cert.getPublicKey.getAlgorithm", fromCert.getAlgorithm()); }
        catch (Throwable t) { p("cert.getPublicKey.getAlgorithm", "THREW " + t); }
        try { p("cert.getPublicKey.getFormat", fromCert.getFormat()); }
        catch (Throwable t) { p("cert.getPublicKey.getFormat", "THREW " + t); }

        PublicKey fromPair = root.getKeyPair().getPublic();
        p("keyPair.getPublic.cls", cls(fromPair));
        p("keyPair.getPublic.getAlgorithm", fromPair.getAlgorithm());
        p("sameObject", fromCert == fromPair);

        // The actual failing operation.
        try {
            X509Bundle leaf = base.copy().ecp256()
                    .subject("CN=leaf.netty.io, O=Netty")
                    .buildIssuedBy(root);
            p("buildIssuedBy", "OK issuer=" + leaf.getCertificate().getIssuerX500Principal());
        } catch (Throwable t) {
            p("buildIssuedBy", "THREW " + t.getClass().getName() + ": " + t.getMessage());
        }
    }
}
