import java.io.ByteArrayInputStream;
import java.math.BigInteger;
import java.security.PublicKey;
import java.security.cert.CertificateExpiredException;
import java.security.cert.CertificateFactory;
import java.security.cert.CertificateNotYetValidException;
import java.security.cert.X509Certificate;
import java.util.Base64;
import java.util.Date;

/**
 * JDK-only corpus: the seventeen natives registered on the ABSTRACT
 * {@code java.security.cert.X509Certificate}, asked of a REAL
 * {@code sun.security.x509.X509CertImpl}.
 *
 * WHY THIS EXISTS. Those natives read a three-field synthetic layout —
 * {@code subject_str} at slot 0, {@code issuer_str} at slot 1, {@code cert_id}
 * at slot 2 — and they are registered on the abstract supertype, because that
 * is where a synthetic certificate mirror's methods resolve. CratonVM's cold
 * dispatch skips the superclass walk only when the receiver's own class
 * declares the method ({@code has_own_bytecode}); a triple the receiver
 * INHERITS is intercepted by the ancestor's native, which then indexes into a
 * layout a real {@code X509CertImpl} does not have.
 *
 * A per-triple {@code javap} of {@code sun.security.x509.X509CertImpl} on
 * OpenJDK 25 settles which of the seventeen can be reached that way. Sixteen
 * are declared by {@code X509CertImpl} itself, so its own bytecode wins and no
 * native is consulted. The seventeenth is {@code getType()}: it is
 * {@code public final} on {@code java.security.cert.Certificate}, two frames
 * up, so {@code X509CertImpl} declares nothing and the native DOES intercept.
 * It answers the constant {@code "X.509"}, which is the same string
 * {@code Certificate.getType()} returns for every X.509 certificate (the
 * {@code X509CertImpl} constructor passes it to {@code super}), so the
 * interception is benign — but it is benign by measurement, not by argument,
 * and this vector is where the measurement lives.
 *
 * The certificate below is a fixed, self-signed RSA-2048 / SHA256withRSA cert
 * with a hard-coded validity window (2020-01-01 to 2119-12-08), so every
 * observable here is a constant rather than a function of the clock or of a
 * freshly generated key.
 *
 * Determinism: no key generation, no clock-dependent assertion except the
 * validity window (which is a century wide), and no identity hashes.
 */
public class RJdkX509Intercept {
    static int checks;

    /** A fixed self-signed cert. Generated once with keytool; never regenerated. */
    private static final String DER_B64 =
            "MIIDLjCCAhagAwIBAgIJAOst96U/cWsGMA0GCSqGSIb3DQEBCwUAMEQxCzAJBgNVBAYTAkdCMREwDwYD"
            + "VQQKEwhDcmF0b25WTTEPMA0GA1UECxMGQ3JhdG9uMREwDwYDVQQDEwhSSmRrWDUwOTAgFw0yMDAxMDEw"
            + "MDAwMDBaGA8yMTE5MTIwODAwMDAwMFowRDELMAkGA1UEBhMCR0IxETAPBgNVBAoTCENyYXRvblZNMQ8w"
            + "DQYDVQQLEwZDcmF0b24xETAPBgNVBAMTCFJKZGtYNTA5MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIB"
            + "CgKCAQEAs9/RtJzPVTvR4eQqjv7ZCBGzBuS3c7fMA+SW1kZLpHd2N2W79NRK2Pc3LGXufoMMKooTh/54"
            + "Kgm1yEoAVSVZVUMPMMHwcfBO5FfdmQYng0yuNz13s0djWwYQNQ2ceMDh9WMXh1R50d3KT5Vu1UjjmcyF"
            + "v43qRp72BcdqT24/+MguP/GBgL2/rQ87G+76YaDHLhHy36L9rOkE8OmlDl3emj43AJRi1f5kVP3Y/o2E"
            + "9oDL3Qw3RbcZimeHpT/wU694BsIWoasRBY5zGbWXUpT5wkTTl59o298VpFrOONgqWlwRqYF3QjbVkIBk"
            + "fBL32E7a5xslliKlr+kW37dMLgqMiQIDAQABoyEwHzAdBgNVHQ4EFgQU5HYaj9uzSGLpi3+2i9CzEswb"
            + "8AowDQYJKoZIhvcNAQELBQADggEBAI6v0SMUw3TtIirh5Pkdm2CuQZUyhPmlT/1NVDP0OQ5I6LsAuoGC"
            + "7+9TnZD9Rq8mdM5S3YEnEWhLaOlR9IvzpFBB8HQt8KjLmTAoZETPrIpZ9yGhZWkV5984X1hGdB1wOqMU"
            + "Ojq0bG8PK5oCcjTxZ1D5emTYJ+gGiR/6aKmtGl8aekBDXNgDGLoGhKlxaJrEkuqsRNICG8s040P7YwXk"
            + "BCqtu9puO3h81RKAH1UfLZ8Ue7IQ6WG4FmGO2jjpMz/5jeH1iTtspnT1kB2WnF4tAcUqsyajXFVHPAjE"
            + "WFvIJM/5hrofCvzrIoItHiB9H3GJfeH98ixovRy/iZi6w8tSxhM=";

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkX509Intercept: " + m);
        }
    }

    static X509Certificate parse() throws Exception {
        byte[] der = Base64.getDecoder().decode(DER_B64);
        return (X509Certificate) CertificateFactory.getInstance("X.509")
                .generateCertificate(new ByteArrayInputStream(der));
    }

    public static void main(String[] args) throws Exception {
        X509Certificate c = parse();
        byte[] der = Base64.getDecoder().decode(DER_B64);

        // The one INHERITED triple: getType() is `public final` on
        // java.security.cert.Certificate, so X509CertImpl declares nothing and
        // the native on the abstract X509Certificate intercepts. It must still
        // answer what Certificate.getType() would.
        check("X.509".equals(c.getType()), "getType() = " + c.getType());
        System.out.println("CK RJdkX509Intercept type=" + c.getType());

        // The sixteen X509CertImpl DECLARES. Its own bytecode should win, so
        // every one of these is a statement about the real certificate's DER
        // rather than about the VM's three-field synthetic layout. A native
        // that reached them would answer from slots 0/1/2 of an object that
        // has neither, which is what makes these worth asserting.
        check(c.getVersion() == 3, "getVersion() = " + c.getVersion());
        check(c.getSerialNumber().equals(new BigInteger("EB2DF7A53F716B06", 16)),
                "getSerialNumber() = " + c.getSerialNumber().toString(16));
        check("SHA256withRSA".equals(c.getSigAlgName()), "getSigAlgName() = " + c.getSigAlgName());
        check("1.2.840.113549.1.1.11".equals(c.getSigAlgOID()), "getSigAlgOID() = " + c.getSigAlgOID());

        String subject = c.getSubjectX500Principal().getName();
        String issuer = c.getIssuerX500Principal().getName();
        check("CN=RJdkX509,OU=Craton,O=CratonVM,C=GB".equals(subject), "subject = " + subject);
        check(subject.equals(issuer), "a self-signed cert's issuer is its subject: " + issuer);
        // getSubjectDN/getIssuerDN return an `X500Name`, whose `getName()`
        // renders with spaces after the commas — deliberately NOT the same
        // string as the X500Principal's RFC 2253 form. Both are printed so the
        // cross-VM diff pins the exact rendering; only the RDN content, which
        // is what a shim reading a synthetic slot would get wrong, is asserted.
        String subjectDN = c.getSubjectDN().getName();
        String issuerDN = c.getIssuerDN().getName();
        check(subjectDN.contains("CN=RJdkX509") && subjectDN.contains("O=CratonVM"),
                "getSubjectDN() = " + subjectDN);
        check(subjectDN.equals(issuerDN), "a self-signed cert's issuer DN is its subject DN");
        System.out.println("CK RJdkX509Intercept subject=" + subject);
        System.out.println("CK RJdkX509Intercept subjectDN=" + subjectDN);

        check(c.getNotBefore().getTime() == 1577836800000L,
                "getNotBefore() = " + c.getNotBefore().getTime());
        check(c.getNotAfter().getTime() == 4731436800000L,
                "getNotAfter() = " + c.getNotAfter().getTime());
        System.out.println("CK RJdkX509Intercept validity=" + c.getNotBefore().getTime()
                + "," + c.getNotAfter().getTime());

        // checkValidity(): silent inside the window, and each boundary throws
        // its OWN exception type — a shim that answers "valid" unconditionally
        // passes the first and fails the other two.
        c.checkValidity();
        c.checkValidity(new Date(2_000_000_000_000L));
        boolean tooEarly = false;
        try {
            c.checkValidity(new Date(0L));
        } catch (CertificateNotYetValidException e) {
            tooEarly = true;
        }
        check(tooEarly, "checkValidity(1970) raises CertificateNotYetValidException");
        boolean tooLate = false;
        try {
            c.checkValidity(new Date(5_000_000_000_000L));
        } catch (CertificateExpiredException e) {
            tooLate = true;
        }
        check(tooLate, "checkValidity(2128) raises CertificateExpiredException");
        System.out.println("CK RJdkX509Intercept validityChecks=ok");

        // getEncoded() must be the DER we handed in, byte for byte. This is the
        // triple that used to answer an EMPTY array in the default build, which
        // turns a real certificate into "no certificate" with no error.
        byte[] enc = c.getEncoded();
        check(enc.length == der.length, "getEncoded() length " + enc.length + " want " + der.length);
        check(java.util.Arrays.equals(enc, der), "getEncoded() must round-trip the DER");
        System.out.println("CK RJdkX509Intercept encodedLen=" + enc.length);

        byte[] tbs = c.getTBSCertificate();
        check(tbs.length > 0 && tbs.length < enc.length, "getTBSCertificate() is a proper prefix-sized part");
        byte[] sig = c.getSignature();
        check(sig.length == 256, "getSignature() of an RSA-2048 cert is 256 bytes, got " + sig.length);
        System.out.println("CK RJdkX509Intercept tbsLen=" + tbs.length + " sigLen=" + sig.length);

        PublicKey pk = c.getPublicKey();
        check("RSA".equals(pk.getAlgorithm()), "getPublicKey().getAlgorithm() = " + pk.getAlgorithm());
        check("X.509".equals(pk.getFormat()), "getPublicKey().getFormat() = " + pk.getFormat());
        check(pk.getEncoded().length == 294,
                "the SPKI encoding of this key is 294 bytes, got " + pk.getEncoded().length);
        System.out.println("CK RJdkX509Intercept pubkey=" + pk.getAlgorithm() + ","
                + pk.getEncoded().length);

        // verify(): a self-signed certificate verifies against its OWN public
        // key, and must NOT verify against a different one.
        //
        // THE SECOND HALF OF THAT SENTENCE WAS ONLY EVER A COMMENT. Until
        // 2026-08-12 this arm was the bare `c.verify(pk)` below and nothing
        // else, so a delegating X509Certificate whose verify() is a no-op — a
        // VM that accepts ANY certificate under ANY key, which is the whole
        // failure mode worth having a certificate vector for — produced
        // identical output and passed (W7-51-vacuous-sweep-round-2.md §2.5).
        // A positive arm alone cannot tell "verified" from "did not look".
        //
        // Two negative controls, and they fail for different reasons, which is
        // why both are here: the first proves the KEY is consulted, the second
        // proves the SIGNATURE BYTES are. A verify() that compared only key
        // identity would pass the second; one that ignored the key entirely
        // would pass the first.
        c.verify(pk);

        // (a) the wrong key. Derived from this certificate's own SPKI with one
        // bit flipped inside the modulus, so it is a well-formed RSA-2048
        // public key of the right size that simply is not this one. No key
        // generation, so the vector stays deterministic and gains no dependency
        // on a working KeyPairGenerator under --jdk-only.
        byte[] spki = pk.getEncoded();
        byte[] bentSpki = spki.clone();
        bentSpki[100] ^= 0x01;
        PublicKey bent = java.security.KeyFactory.getInstance("RSA")
                .generatePublic(new java.security.spec.X509EncodedKeySpec(bentSpki));
        check(!java.util.Arrays.equals(bent.getEncoded(), spki),
                "the bent key must actually differ from the certificate's own");
        String wrongKey = "VERIFIED";
        try {
            c.verify(bent);
        } catch (Exception e) {
            wrongKey = e.getClass().getSimpleName();
        }
        check(!wrongKey.equals("VERIFIED"),
                "a certificate must NOT verify against a different public key");

        // (b) the wrong bytes. One bit flipped inside the signature BIT STRING,
        // the last element of the DER, so the certificate still parses and only
        // its signature is wrong.
        byte[] bentDer = der.clone();
        bentDer[bentDer.length - 10] ^= 0x01;
        X509Certificate tampered = (X509Certificate) CertificateFactory.getInstance("X.509")
                .generateCertificate(new ByteArrayInputStream(bentDer));
        check(!java.util.Arrays.equals(tampered.getSignature(), c.getSignature()),
                "the tampered certificate must carry a different signature");
        String tamperedOutcome = "VERIFIED";
        try {
            tampered.verify(tampered.getPublicKey());
        } catch (Exception e) {
            tamperedOutcome = e.getClass().getSimpleName();
        }
        check(!tamperedOutcome.equals("VERIFIED"),
                "a certificate with a tampered signature must NOT verify");

        System.out.println("CK RJdkX509Intercept verify=ok wrongKey=" + wrongKey
                + " tampered=" + tamperedOutcome);

        // Two certificates parsed from the same DER are equal and hash alike —
        // both are Certificate bytecode over getEncoded(), so a getEncoded()
        // that answered empty would make ANY two certificates equal.
        X509Certificate c2 = parse();
        check(c.equals(c2), "two parses of the same DER are equal");
        check(c.hashCode() == c2.hashCode(), "…and hash alike");
        System.out.println("CK RJdkX509Intercept equality=ok");

        System.out.println("CK RJdkX509Intercept checks=" + checks);
        System.out.println("PASS RJdkX509Intercept (" + checks + " checks)");
    }
}
