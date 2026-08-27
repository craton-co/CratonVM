import java.io.FileInputStream;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.List;

/**
 * `sun.security.util.HostnameChecker` in isolation, on one certificate.
 *
 * netty's OpenSSL provider hands endpoint identification to the JDK's
 * `X509TrustManagerImpl.checkIdentity`, which ends in
 * `HostnameChecker.getInstance(TYPE_TLS).match(hostname, cert, chainsToPublicCA)`.
 * With every input measured identical on both VMs — `peerHost=localhost`,
 * `endpointIdentificationAlgorithm=HTTPS`, an extended handshake session, and a
 * peer certificate whose subject is `CN=NOTlocalhost` — HotSpot's trust manager
 * threw `CertificateException: No name matching localhost found` and CratonVM's
 * returned normally (`probes/OpenSslEndpointIdentProbe.java`). This probe takes
 * the trust manager, the engine and netty out of the picture and asks the
 * checker directly, and prints the two inputs it decides on: the certificate's
 * subject alternative names and its subject DN.
 *
 * Needs `--add-exports java.base/sun.security.util=ALL-UNNAMED`.
 *
 * usage: HostnameCheckerProbe <cert.pem> <hostname> [expect-match|expect-reject]
 */
public final class HostnameCheckerProbe {
    private HostnameCheckerProbe() {}

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            System.err.println("usage: HostnameCheckerProbe <cert.pem> <hostname> "
                    + "[expect-match|expect-reject]");
            System.exit(2);
        }
        String certPath = args[0];
        String hostname = args[1];
        boolean expectMatch = args.length > 2 && "expect-match".equals(args[2]);

        CertificateFactory cf = CertificateFactory.getInstance("X.509");
        X509Certificate cert;
        try (FileInputStream in = new FileInputStream(certPath)) {
            cert = (X509Certificate) cf.generateCertificate(in);
        }

        System.out.println("@@CERT subject=" + cert.getSubjectX500Principal().getName()
                + " issuer=" + cert.getIssuerX500Principal().getName());
        Object sans;
        try {
            sans = cert.getSubjectAlternativeNames();
        } catch (Exception e) {
            sans = "<threw " + e + ">";
        }
        System.out.println("@@CERT subjectAlternativeNames=" + sans);
        if (sans instanceof java.util.Collection) {
            for (Object o : (java.util.Collection<?>) sans) {
                List<?> entry = (List<?>) o;
                System.out.println("@@SAN type=" + entry.get(0) + " value=" + entry.get(1)
                        + " valueClass=" + (entry.get(1) == null
                                ? "null" : entry.get(1).getClass().getName()));
            }
        }

        boolean matched;
        String detail;
        try {
            sun.security.util.HostnameChecker
                    .getInstance(sun.security.util.HostnameChecker.TYPE_TLS)
                    .match(hostname, cert, false);
            matched = true;
            detail = "returned normally";
        } catch (Exception e) {
            matched = false;
            detail = e.getClass().getName() + ": " + e.getMessage();
        }
        boolean ok = matched == expectMatch;
        System.out.println("@@MATCH " + (ok ? "PASS" : "FAIL")
                + " hostname=" + hostname + " matched=" + matched
                + " (want " + expectMatch + ") detail=" + detail);
        if (!ok) {
            System.exit(1);
        }
    }
}
