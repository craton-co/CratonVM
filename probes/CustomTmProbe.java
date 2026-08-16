import java.net.InetSocketAddress;
import java.security.KeyStore;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;

/**
 * The app-visible consequence of a leaf-only peer chain.
 *
 * An `SSLContext` initialised with real `TrustManager`s is the shape every
 * HTTP client library uses (HttpClient5's `SSLContextBuilder`, netty's
 * `SslContextBuilder.trustManager(...)`). On CratonVM that path stands the
 * native verifier down and makes the Java TrustManager the ONLY verifier —
 * which is correct, and only works if the TrustManager is handed the chain
 * the peer actually sent.
 *
 * Against a real public site the chain is 2-4 certificates. If the VM captured
 * only the leaf, the TrustManager cannot build a path to any root and the
 * connection fails — while the SAME context on HotSpot connects.
 *
 * Prints, per host, what the default factory does (no custom TMs) and what
 * the custom-TM context does, so a difference between the two isolates the
 * chain rather than the trust store.
 */
public class CustomTmProbe {
    static final String[] HOSTS = { "github.com", "www.google.com", "repo.maven.apache.org" };

    public static void main(String[] a) throws Exception {
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init((KeyStore) null);
        TrustManager[] tms = tmf.getTrustManagers();
        SSLContext custom = SSLContext.getInstance("TLS");
        custom.init(null, tms, null);

        for (String host : HOSTS) {
            System.out.println("HOST " + host);
            System.out.println("   default-factory : " + attempt((SSLSocketFactory) SSLSocketFactory.getDefault(), host));
            System.out.println("   custom-TM-ctx   : " + attempt(custom.getSocketFactory(), host));
        }
        System.out.println("PROBE-DONE");
        Runtime.getRuntime().halt(0);
    }

    static String attempt(SSLSocketFactory f, String host) {
        try {
            SSLSocket s = (SSLSocket) f.createSocket();
            s.connect(new InetSocketAddress(host, 443), 10000);
            s.setSoTimeout(10000);
            s.startHandshake();
            int n = s.getSession().getPeerCertificates().length;
            return "OK  peerChainLen=" + n;
        } catch (Throwable e) {
            String m = e.getMessage();
            if (m != null && m.length() > 100) {
                m = m.substring(0, 100);
            }
            return "FAIL " + e.getClass().getSimpleName() + ": " + m;
        }
    }
}
