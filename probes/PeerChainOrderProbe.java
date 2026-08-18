import java.net.InetSocketAddress;
import java.security.cert.Certificate;
import java.security.cert.X509Certificate;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

/**
 * `SSLSession.getPeerCertificates()` is ORDERED, and a certificate store is
 * not.
 *
 * JSSE specifies the peer's own certificate first, then each issuer in turn.
 * On Unix the VM reads the chain from `SSL_get_peer_cert_chain`, which is a
 * list in the order the peer sent it. On Windows it comes out of SChannel's
 * attached CertStore, which is a SET — `certs()` enumerates it in whatever
 * order the store holds, so the path has to be walked rather than hoped for.
 *
 * A chain that is merely COMPLETE passes `RealChainProbe` either way, because
 * `x509_manager::validate_chain` builds its own path before validating. The
 * ordering is only visible to an application that reads the array, which is
 * exactly what certificate pinning and chain inspection do — so it needs its
 * own measurement, against HotSpot, and this is it.
 *
 * Prints one line per host: the length, whether the array is a proper chain
 * (each certificate's issuer is the next one's subject), and the subject CNs
 * in order. Diff the two VMs.
 */
public class PeerChainOrderProbe {
    static final String[] HOSTS = {
        "github.com", "www.google.com", "repo.maven.apache.org",
        "en.wikipedia.org", "www.cloudflare.com", "letsencrypt.org",
    };

    public static void main(String[] a) throws Exception {
        for (String host : HOSTS) {
            System.out.println("HOST " + host + " " + describe(host));
        }
        System.out.println("PROBE-DONE");
        Runtime.getRuntime().halt(0);
    }

    static String describe(String host) {
        try {
            SSLSocket s = (SSLSocket) SSLSocketFactory.getDefault().createSocket();
            s.connect(new InetSocketAddress(host, 443), 10000);
            s.setSoTimeout(10000);
            s.startHandshake();
            Certificate[] peer = s.getSession().getPeerCertificates();
            StringBuilder sb = new StringBuilder();
            boolean ordered = true;
            for (int i = 0; i < peer.length; i++) {
                X509Certificate c = (X509Certificate) peer[i];
                if (i > 0) {
                    sb.append(" -> ");
                    X509Certificate prev = (X509Certificate) peer[i - 1];
                    // The array is a chain only if each certificate is issued
                    // by the one after it. Compared as X500Principal rather
                    // than as a string: the two VMs render the same DN
                    // differently (hex-escaped OIDs vs keyword forms), and a
                    // string compare would report a false difference.
                    if (!prev.getIssuerX500Principal().equals(c.getSubjectX500Principal())) {
                        ordered = false;
                    }
                }
                sb.append(cn(c));
            }
            return "len=" + peer.length + " ordered=" + ordered + " : " + sb;
        } catch (Throwable e) {
            return "FAIL " + e.getClass().getSimpleName() + ": " + e.getMessage();
        }
    }

    /** The CN alone, so the line is comparable across two DN renderings. */
    static String cn(X509Certificate c) {
        String dn = c.getSubjectX500Principal().getName();
        for (String part : dn.split(",")) {
            String p = part.trim();
            if (p.startsWith("CN=")) {
                return p.substring(3);
            }
        }
        return dn;
    }
}
