import java.io.File;
import java.security.KeyStore;
import java.security.cert.X509Certificate;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;

/**
 * WHICH default trust anchors does each VM actually have?
 *
 * JSSE's default trust store is `$JAVA_HOME/lib/security/cacerts`. CratonVM
 * builds its id-0 anchor set from the OS trust store instead. If those sets
 * differ, then with no `javax.net.ssl.trustStore` configured the two VMs
 * trust different CAs — which decides real connections, not just error text.
 *
 * Prints one sorted subject line per anchor so the two runs can be diffed.
 */
public class TrustSetProbe {
    public static void main(String[] a) throws Exception {
        System.out.println("java.home=" + System.getProperty("java.home"));
        File cacerts = new File(System.getProperty("java.home"), "lib/security/cacerts");
        System.out.println("cacerts.exists=" + cacerts.exists() + " bytes="
                + (cacerts.exists() ? cacerts.length() : -1));
        System.out.println("trustStoreProperty="
                + System.getProperty("javax.net.ssl.trustStore"));

        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init((KeyStore) null);
        X509TrustManager x = null;
        for (TrustManager tm : tmf.getTrustManagers()) {
            if (tm instanceof X509TrustManager) {
                x = (X509TrustManager) tm;
                break;
            }
        }
        X509Certificate[] issuers = x.getAcceptedIssuers();
        System.out.println("ANCHORS=" + issuers.length);
        List<String> subjects = new ArrayList<>();
        for (X509Certificate c : issuers) {
            // Fingerprint, NOT the subject string: the two VMs render the
            // same DN differently (hex-escaped OIDs vs EMAILADDRESS=/
            // SERIALNUMBER= keywords), so a subject-keyed diff reports the
            // same certificate as two different anchors.
            java.security.MessageDigest md = java.security.MessageDigest.getInstance("SHA-256");
            byte[] fp = md.digest(c.getEncoded());
            StringBuilder h = new StringBuilder();
            for (byte b : fp) {
                h.append(String.format("%02x", b));
            }
            subjects.add(h.substring(0, 32) + "  " + c.getSubjectX500Principal().getName()
                    + " |keyAlg=" + c.getPublicKey().getAlgorithm());
        }
        Collections.sort(subjects);
        for (String s : subjects) {
            System.out.println("ANCHOR " + s);
        }
        System.out.println("PROBE-DONE");
        System.exit(0);
    }
}
