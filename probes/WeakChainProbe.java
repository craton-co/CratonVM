import java.net.InetSocketAddress;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

/**
 * The witness for the last residue: a chain that terminates at an anchor in
 * the DEFAULT trust store (this JDK image's cacerts) whose leaf carries a
 * 1024-bit RSA key.
 *
 * The JDK's floor is 1024 bits (`jdk.certpath.disabledAlgorithms` disables
 * RSA below that), so JSSE completes the handshake. OpenSSL's default security
 * level of 2 requires 2048 and refuses — which is what a CratonVM client with
 * no `javax.net.ssl.trustStore` configured still goes through.
 *
 * NO trust store property is set here on purpose: setting one would take the
 * explicitly-configured path, which already bypasses the security level.
 *
 * `startHandshake()` and nothing else. An earlier cut wrote a byte and then
 * closed, and hung — not in the handshake (a JSSE debug log showed it
 * completing) but in `close()`, against an `s_server -www` still waiting for
 * the rest of an HTTP request. Read, write and close are all fixture-coupled;
 * the handshake is the only thing under test, so it is the only thing done.
 */
public class WeakChainProbe {
    public static void main(String[] a) throws Exception {
        int port = a.length > 0 ? Integer.parseInt(a[0]) : 9801;
        System.out.println("trustStoreProperty="
                + System.getProperty("javax.net.ssl.trustStore"));
        System.out.println("java.home=" + System.getProperty("java.home"));
        long t0 = System.currentTimeMillis();
        String verdict;
        try {
            SSLSocket s = (SSLSocket) SSLSocketFactory.getDefault().createSocket();
            s.connect(new InetSocketAddress("127.0.0.1", port), 10000);
            s.setSoTimeout(10000);
            s.startHandshake();
            verdict = "HANDSHAKE-OK cipher=" + s.getSession().getCipherSuite();
        } catch (Throwable e) {
            StringBuilder sb = new StringBuilder();
            for (Throwable x = e; x != null; x = x.getCause()) {
                sb.append(x.getClass().getName()).append(": ").append(x.getMessage()).append(" | ");
                if (x.getCause() == x) {
                    break;
                }
            }
            verdict = "REFUSED " + sb;
        }
        System.out.println("CLIENT ms=" + (System.currentTimeMillis() - t0) + " -> " + verdict);
        System.out.println("PROBE-DONE");
        Runtime.getRuntime().halt(0);
    }
}
