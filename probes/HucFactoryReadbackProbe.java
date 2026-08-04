import java.net.URL;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocketFactory;

/**
 * The HttpsURLConnection SSLSocketFactory round trip, both halves.
 *
 * READBACK: getSSLSocketFactory() must return the very object
 * setSSLSocketFactory() installed. ISOLATION: the setter is scoped to ONE
 * connection, so a second connection must not observe the first one's factory
 * — that pair is what makes reading a per-connection field, rather than a
 * process-wide slot, load-bearing. Passing only one of the two is easy and
 * wrong: a no-op setter passes isolation, and a process-wide slot passes
 * readback.
 *
 * Also pins the default factory's identity stability. The JDK documents
 * SSLSocketFactory.getDefault() as returning *the* default factory, and every
 * identity-keyed table over a factory (trust anchors, client identity) is only
 * findable again if that identity holds still.
 *
 * Every expectation here was first measured on real JDK 21, which passes all
 * of them.
 */
public class HucFactoryReadbackProbe {
    private static final String URL_STR = "https://repo.maven.apache.org/maven2/";
    private static int pass = 0;
    private static int fail = 0;

    public static void main(String[] args) throws Exception {
        SSLSocketFactory mine = freshFactory();
        SSLSocketFactory other = freshFactory();
        report("two SSLContexts yield distinct factories", mine != other,
               "both were " + id(mine));

        // --- readback + isolation ---------------------------------------
        HttpsURLConnection a = open();
        HttpsURLConnection b = open();

        SSLSocketFactory bBefore = b.getSSLSocketFactory();
        a.setSSLSocketFactory(mine);

        SSLSocketFactory aBack = a.getSSLSocketFactory();
        report("READBACK: getSSLSocketFactory returns the installed object",
               aBack == mine, "installed " + id(mine) + ", read back " + id(aBack));

        report("READBACK is stable across repeated reads",
               a.getSSLSocketFactory() == mine, "second read was " + id(a.getSSLSocketFactory()));

        report("ISOLATION: a second connection does not see it",
               b.getSSLSocketFactory() != mine,
               "connection b also reported " + id(mine));

        report("ISOLATION: the untouched connection keeps its prior factory",
               b.getSSLSocketFactory() == bBefore,
               "was " + id(bBefore) + ", now " + id(b.getSSLSocketFactory()));

        // Overwriting must take, not merge.
        a.setSSLSocketFactory(other);
        report("re-setting replaces the installed factory",
               a.getSSLSocketFactory() == other,
               "expected " + id(other) + ", got " + id(a.getSSLSocketFactory()));

        // --- JDK-documented null rejection -------------------------------
        try {
            a.setSSLSocketFactory(null);
            report("setSSLSocketFactory(null) throws IllegalArgumentException", false,
                   "no exception thrown");
        } catch (IllegalArgumentException expected) {
            report("setSSLSocketFactory(null) throws IllegalArgumentException", true, "");
        } catch (Throwable t) {
            report("setSSLSocketFactory(null) throws IllegalArgumentException", false,
                   "threw " + t.getClass().getName());
        }
        report("a rejected null leaves the previous factory installed",
               a.getSSLSocketFactory() == other,
               "factory is now " + id(a.getSSLSocketFactory()));

        // --- default factory identity ------------------------------------
        // Measured on real JDK 21, and the two layers genuinely differ:
        // SSLSocketFactory.getDefault() news up an SSLSocketFactoryImpl per
        // call, while HttpsURLConnection caches its default in its own static
        // field on first use. Assert the CACHING one — asserting the other
        // would be asserting a divergence from the JDK.
        SSLSocketFactory h1 = HttpsURLConnection.getDefaultSSLSocketFactory();
        SSLSocketFactory h2 = HttpsURLConnection.getDefaultSSLSocketFactory();
        report("HttpsURLConnection.getDefaultSSLSocketFactory() is stable", h1 == h2,
               id(h1) + " vs " + id(h2));

        report("an untouched connection reports the same factory as another untouched one",
               open().getSSLSocketFactory() == open().getSSLSocketFactory(),
               "two fresh connections disagreed");

        // --- an installed factory must still be able to connect ----------
        HttpsURLConnection c = open();
        c.setSSLSocketFactory(freshFactory());
        c.setConnectTimeout(15000);
        c.setReadTimeout(20000);
        int code = c.getResponseCode();
        report("a connection with an instance factory still completes the request",
               code == 200, "response code " + code);
        c.disconnect();

        System.out.println("PROBE-SUMMARY pass=" + pass + " fail=" + fail);
        System.out.println(fail == 0 ? "PROBE-RESULT=PASS" : "PROBE-RESULT=FAIL");
        if (fail != 0) {
            System.exit(2);
        }
    }

    private static HttpsURLConnection open() throws Exception {
        return (HttpsURLConnection) new URL(URL_STR).openConnection();
    }

    private static SSLSocketFactory freshFactory() throws Exception {
        SSLContext c = SSLContext.getInstance("TLS");
        c.init(null, null, null);
        return c.getSocketFactory();
    }

    private static String id(Object o) {
        return o == null ? "null" : Integer.toString(System.identityHashCode(o));
    }

    private static void report(String label, boolean ok, String detail) {
        if (ok) {
            pass++;
            System.out.println("  ok   " + label);
        } else {
            fail++;
            System.out.println("  FAIL " + label + (detail.isEmpty() ? "" : " -- " + detail));
        }
    }
}
