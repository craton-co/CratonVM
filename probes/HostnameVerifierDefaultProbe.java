import java.net.URL;
import javax.net.ssl.HostnameVerifier;
import javax.net.ssl.HttpsURLConnection;

/**
 * Which `HostnameVerifier` is actually in force for an `HttpsURLConnection`?
 *
 * TWO DIFFERENT READS, and they do not agree — which is the whole reason this
 * probe exists rather than a one-liner:
 *
 *   1. `HttpsURLConnection.getDefaultHostnameVerifier()` — the process-wide
 *      static. On CratonVM this is answered by a native that returns the VM's
 *      own bare-interface synthetic when nothing was ever installed.
 *   2. `connection.getHostnameVerifier()` — the per-instance field, which the
 *      REAL JDK `HttpsURLConnection` constructor initialises from its own
 *      static. That static is set by `HttpsURLConnection.<clinit>` to
 *      `HttpsURLConnection$DefaultHostnameVerifier`, whose entire body is
 *      `iconst_0; ireturn` — it ALWAYS answers `false`.
 *
 * Read (2) is the one `http_url_connection::huc_hostname_verifier` consults
 * first, so read (2) is the one that decides a request. On HotSpot that
 * hardcoded `false` is harmless: real JSSE only consults a `HostnameVerifier`
 * as a FALLBACK, after its own RFC 2818 endpoint identification has already
 * FAILED (`sun.net.www.protocol.https.HttpsClient.checkURLSpoofing`), and when
 * the default verifier is the one installed it does the check in-handshake and
 * never calls it at all. A VM that instead calls it unconditionally and reads
 * non-`true` as a rejection rejects every single https connection.
 *
 * The `verify(...)` verdicts printed below are the point: a `false` here for a
 * plain `localhost` is CORRECT JDK behaviour, not a bug in the verifier.
 */
public class HostnameVerifierDefaultProbe {
    public static void main(String[] args) throws Exception {
        report("static  HttpsURLConnection.getDefaultHostnameVerifier()",
                HttpsURLConnection.getDefaultHostnameVerifier());

        // No connect() — constructing the connection is enough to populate the
        // instance field, and this probe must not need a live TLS peer.
        Object conn = new URL("https://localhost:1/").openConnection();
        if (conn instanceof HttpsURLConnection https) {
            report("instance " + conn.getClass().getName() + ".getHostnameVerifier()",
                    https.getHostnameVerifier());
        } else {
            System.out.println("instance: openConnection gave " + conn.getClass().getName()
                    + ", not an HttpsURLConnection");
        }
    }

    private static void report(String what, HostnameVerifier hv) {
        System.out.println(what);
        System.out.println("    class  = " + (hv == null ? "<null>" : hv.getClass().getName()));
        if (hv == null) {
            return;
        }
        try {
            System.out.println("    verify(\"localhost\", null) = " + hv.verify("localhost", null));
        } catch (Throwable t) {
            System.out.println("    verify(\"localhost\", null) threw " + t);
        }
    }
}
