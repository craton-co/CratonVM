import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.InetSocketAddress;
import java.net.Socket;
import java.net.URL;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

/**
 * Every way a caller can obtain an SSLSocketFactory, exercised through the
 * layered createSocket(Socket,String,int,boolean) overload that reads the
 * factory's owning SSLContext.
 *
 * Companion to SsfDefaultProbe (which covers only the static getDefault()
 * path that broke Aether). This one also covers the HttpsURLConnection
 * getters and the setDefault/getDefault round trip, so a change to any of
 * them shows up as a named check rather than as a distant suite failure.
 */
public class SsfSurfaceProbe {
    private static final String HOST = "repo.maven.apache.org";
    private static final int PORT = 443;
    private static final String PATH =
        "/maven2/com/google/code/gson/gson/2.10/gson-2.10.pom";

    private static int pass = 0;
    private static int fail = 0;

    public static void main(String[] args) throws Exception {
        check("SSLSocketFactory.getDefault", (SSLSocketFactory) SSLSocketFactory.getDefault());

        check("SSLContext.getInstance(TLS).getSocketFactory",
              tlsContextFactory());

        check("HttpsURLConnection.getDefaultSSLSocketFactory (unset)",
              HttpsURLConnection.getDefaultSSLSocketFactory());

        URL url = new URL("https://" + HOST + PATH);
        HttpsURLConnection conn = (HttpsURLConnection) url.openConnection();
        check("HttpsURLConnection.getSSLSocketFactory (instance)",
              conn.getSSLSocketFactory());

        // setDefault/getDefault must round-trip the SAME object (the JDK
        // contract the tls-handshake-enforcement-gap fix restored).
        SSLSocketFactory mine = tlsContextFactory();
        SSLSocketFactory prior = HttpsURLConnection.getDefaultSSLSocketFactory();
        try {
            HttpsURLConnection.setDefaultSSLSocketFactory(mine);
            SSLSocketFactory back = HttpsURLConnection.getDefaultSSLSocketFactory();
            report("setDefaultSSLSocketFactory round-trips the same object",
                   back == mine, "got " + (back == null ? "null" : back.toString()));
            check("HttpsURLConnection.getDefaultSSLSocketFactory (after set)", back);
        } finally {
            if (prior != null) {
                HttpsURLConnection.setDefaultSSLSocketFactory(prior);
            }
        }

        // The ordinary HttpsURLConnection request path must still work.
        HttpsURLConnection req = (HttpsURLConnection) new URL("https://" + HOST + PATH)
            .openConnection();
        req.setConnectTimeout(15000);
        req.setReadTimeout(20000);
        int code = req.getResponseCode();
        report("HttpsURLConnection GET returns 200", code == 200, "code=" + code);
        req.disconnect();

        System.out.println("PROBE-SUMMARY pass=" + pass + " fail=" + fail);
        System.out.println(fail == 0 ? "PROBE-RESULT=PASS" : "PROBE-RESULT=FAIL");
        if (fail != 0) {
            System.exit(2);
        }
    }

    private static SSLSocketFactory tlsContextFactory() throws Exception {
        SSLContext c = SSLContext.getInstance("TLS");
        c.init(null, null, null);
        return c.getSocketFactory();
    }

    /** Drive the layered overload and a real handshake through {@code f}. */
    private static void check(String label, SSLSocketFactory f) {
        if (f == null) {
            report(label, false, "factory is null");
            return;
        }
        Socket plain = null;
        try {
            plain = new Socket();
            plain.connect(new InetSocketAddress(HOST, PORT), 15000);
            plain.setSoTimeout(20000);
            SSLSocket ssl = (SSLSocket) f.createSocket(plain, HOST, PORT, true);
            ssl.startHandshake();
            OutputStream out = ssl.getOutputStream();
            out.write(("GET " + PATH + " HTTP/1.1\r\nHost: " + HOST
                      + "\r\nConnection: close\r\n\r\n").getBytes("US-ASCII"));
            out.flush();
            BufferedReader in = new BufferedReader(
                new InputStreamReader(ssl.getInputStream(), "US-ASCII"));
            String status = in.readLine();
            ssl.close();
            report(label, status != null && status.startsWith("HTTP/1.1 200"),
                   "status=" + status);
        } catch (Exception e) {
            report(label, false, e.getClass().getName() + ": " + e.getMessage());
            if (plain != null) {
                try {
                    plain.close();
                } catch (Exception ignored) {
                    // closing a socket we are already reporting a failure for
                }
            }
        }
    }

    private static void report(String label, boolean ok, String detail) {
        if (ok) {
            pass++;
            System.out.println("  ok   " + label);
        } else {
            fail++;
            System.out.println("  FAIL " + label + " -- " + detail);
        }
    }
}
