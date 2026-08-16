import java.net.InetSocketAddress;
import java.security.KeyStore;
import java.security.cert.Certificate;
import java.security.cert.X509Certificate;
import java.util.ArrayList;
import java.util.List;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;
import javax.net.ssl.TrustManager;
import javax.net.ssl.TrustManagerFactory;
import javax.net.ssl.X509TrustManager;

/**
 * Would moving the default client path off OpenSSL's verifier onto the VM's
 * own `validate_chain` break real connections?
 *
 * Measures both on the SAME run and the SAME chain: complete a handshake with
 * the default factory (whatever verifier that path uses today), take the peer
 * chain out of the session, and hand it to the DEFAULT TrustManager — which
 * on CratonVM is `x509_manager::validate_chain` against cacerts, i.e. exactly
 * the verifier a switch would install.
 *
 * HANDSHAKE=ok VALIDATOR=REJECT on a real site is the answer that stops the
 * switch: it names a chain the internet serves and that validator cannot
 * accept.
 */
public class RealChainProbe {
    static final String[] HOSTS = {
        "www.google.com", "github.com", "www.cloudflare.com", "en.wikipedia.org",
        "www.amazon.com", "www.microsoft.com", "www.apple.com", "aws.amazon.com",
        "repo.maven.apache.org", "central.sonatype.com", "crates.io", "www.rust-lang.org",
        "openjdk.org", "adoptium.net", "www.debian.org", "letsencrypt.org",
        "www.bbc.co.uk", "www.nytimes.com", "stackoverflow.com", "www.digicert.com",
    };

    public static void main(String[] a) throws Exception {
        X509TrustManager tm = defaultTrustManager();
        int hsOk = 0;
        int hsFail = 0;
        int vOk = 0;
        int vFail = 0;
        int skipped = 0;
        List<String> disagreements = new ArrayList<>();
        for (String host : HOSTS) {
            String hs;
            String vd;
            X509Certificate[] chain = null;
            try {
                SSLSocket s = (SSLSocket) SSLSocketFactory.getDefault().createSocket();
                s.connect(new InetSocketAddress(host, 443), 10000);
                s.setSoTimeout(10000);
                s.startHandshake();
                Certificate[] peer = s.getSession().getPeerCertificates();
                chain = new X509Certificate[peer.length];
                for (int i = 0; i < peer.length; i++) {
                    chain[i] = (X509Certificate) peer[i];
                }
                hs = "ok";
                hsOk++;
            } catch (Throwable e) {
                hs = "FAIL(" + e.getClass().getSimpleName() + ")";
                hsFail++;
            }
            if (chain == null || chain.length == 0) {
                vd = "skip";
                skipped++;
            } else {
                try {
                    // NOT the key algorithm: `checkServerTrusted`'s authType
                    // is the KEY-EXCHANGE name, and HotSpot rejects an
                    // unrecognised one with "Unknown authType: EC" before it
                    // looks at the chain at all — which reads as a validator
                    // disagreement and is really a probe bug.
                    String keyAlg = chain[0].getPublicKey().getAlgorithm();
                    String authType = "EC".equals(keyAlg) ? "ECDHE_ECDSA" : "RSA";
                    tm.checkServerTrusted(chain, authType);
                    vd = "ACCEPT";
                    vOk++;
                } catch (Throwable e) {
                    vd = "REJECT(" + firstLine(e.getMessage()) + ")";
                    vFail++;
                    disagreements.add(host + " chainLen=" + chain.length + " " + vd);
                }
            }
            System.out.println("HOST " + pad(host) + " handshake=" + pad2(hs)
                    + " validator=" + vd);
        }
        System.out.println("SUMMARY handshake ok=" + hsOk + " fail=" + hsFail
                + " | validator accept=" + vOk + " reject=" + vFail + " skip=" + skipped);
        for (String d : disagreements) {
            System.out.println("DISAGREE " + d);
        }
        System.out.println("PROBE-DONE");
        Runtime.getRuntime().halt(0);
    }

    static X509TrustManager defaultTrustManager() throws Exception {
        TrustManagerFactory tmf =
                TrustManagerFactory.getInstance(TrustManagerFactory.getDefaultAlgorithm());
        tmf.init((KeyStore) null);
        for (TrustManager tm : tmf.getTrustManagers()) {
            if (tm instanceof X509TrustManager) {
                return (X509TrustManager) tm;
            }
        }
        throw new IllegalStateException("no X509TrustManager");
    }

    static String pad(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 24) {
            b.append(' ');
        }
        return b.toString();
    }

    static String pad2(String s) {
        StringBuilder b = new StringBuilder(s);
        while (b.length() < 14) {
            b.append(' ');
        }
        return b.toString();
    }

    static String firstLine(String s) {
        if (s == null) {
            return "null";
        }
        int nl = s.indexOf('\n');
        String t = nl < 0 ? s : s.substring(0, nl);
        return t.length() > 90 ? t.substring(0, 90) : t;
    }
}
