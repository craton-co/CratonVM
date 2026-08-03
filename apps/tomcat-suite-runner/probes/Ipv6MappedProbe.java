import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.URI;
import java.net.URL;
import java.nio.channels.SocketChannel;
import java.util.ArrayList;
import java.util.List;

/**
 * Regression probe for connecting to the IPv4-mapped IPv6 loopback
 * {@code ::ffff:127.0.0.1}.
 *
 * <p>On Windows an AF_INET6 socket has {@code IPV6_V6ONLY} set by default, so
 * connecting it to a v4-mapped destination fails with WSAEADDRNOTAVAIL
 * (os error 10049). Linux defaults that option off, which is why the same code
 * works there. Java hides this: {@code InetAddress.getByName("::ffff:127.0.0.1")}
 * returns an {@code Inet4Address}, so the JDK never builds an AF_INET6 socket
 * for this destination in the first place.
 *
 * <p>Any CratonVM path that re-parses the destination in Rust — rather than
 * going through {@code InetAddress} — skips that collapse and inherits the
 * platform behaviour. That is how
 * {@code TestStartupIPv6Connectors.testIPv6MappedIPv4} failed:
 *
 * <pre>
 * java.io.IOException: HttpURLConnection response failed:
 *     connect [::ffff:127.0.0.1]:54718: ... (os error 10049)
 * </pre>
 *
 * <p>Each check runs against a loopback listener bound to {@code 127.0.0.1},
 * once via the v4-mapped literal and once via plain {@code 127.0.0.1} as a
 * control — so a failure means "this path mishandles the mapped form", not
 * "the network is down". Exits non-zero if any check fails.
 *
 * <pre>
 * cratonvm.exe -cp &lt;dir&gt; Ipv6MappedProbe
 * java         -cp &lt;dir&gt; Ipv6MappedProbe   # HotSpot control
 * </pre>
 */
public class Ipv6MappedProbe {

    private static final String MAPPED = "::ffff:127.0.0.1";
    private static final List<String> failures = new ArrayList<>();

    private static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "PASS " : "FAIL ") + what + (detail.isEmpty() ? "" : " -- " + detail));
        if (!ok) {
            failures.add(what);
        }
    }

    /** A listener on 127.0.0.1 that accepts one connection and answers HTTP 200. */
    private static ServerSocket httpListener() throws IOException {
        ServerSocket ss = new ServerSocket();
        ss.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
        Thread t = new Thread(() -> {
            while (!ss.isClosed()) {
                try (Socket s = ss.accept()) {
                    InputStream in = s.getInputStream();
                    // Read the request head; stop at the blank line.
                    int state = 0;
                    int b;
                    while (state < 4 && (b = in.read()) != -1) {
                        if ((state % 2 == 0 && b == '\r') || (state % 2 == 1 && b == '\n')) {
                            state++;
                        } else {
                            state = 0;
                        }
                    }
                    OutputStream out = s.getOutputStream();
                    out.write("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nhi"
                            .getBytes("US-ASCII"));
                    out.flush();
                } catch (IOException stop) {
                    return;
                }
            }
        });
        t.setDaemon(true);
        t.start();
        return ss;
    }

    public static void main(String[] args) throws Exception {
        // 1. What does the JDK's own parser make of the literal? HotSpot
        //    collapses it to an Inet4Address; anything else is already a
        //    divergence, even if the connects below happen to work.
        InetAddress mapped = InetAddress.getByName(MAPPED);
        check("InetAddress.getByName(\"" + MAPPED + "\") is an Inet4Address",
                mapped instanceof java.net.Inet4Address,
                mapped.getClass().getName() + " / " + mapped.getHostAddress());

        // 2. Blocking java.net.Socket.
        for (String host : new String[] { "127.0.0.1", MAPPED }) {
            try (ServerSocket ss = new ServerSocket()) {
                ss.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
                try (Socket s = new Socket()) {
                    s.connect(new InetSocketAddress(host, ss.getLocalPort()), 5000);
                    check("Socket.connect to " + host, s.isConnected(), "");
                } catch (Throwable t) {
                    check("Socket.connect to " + host, false, t.toString());
                }
            }
        }

        // 3. Non-blocking-capable SocketChannel (a different connect path).
        for (String host : new String[] { "127.0.0.1", MAPPED }) {
            try (ServerSocket ss = new ServerSocket()) {
                ss.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
                try (SocketChannel ch = SocketChannel.open()) {
                    ch.connect(new InetSocketAddress(host, ss.getLocalPort()));
                    check("SocketChannel.connect to " + host, ch.isConnected(), "");
                } catch (Throwable t) {
                    check("SocketChannel.connect to " + host, false, t.toString());
                }
            }
        }

        // 4. HttpURLConnection — the path TestStartupIPv6Connectors uses. The
        //    URL carries the literal as a STRING, so a client that re-parses it
        //    natively never sees InetAddress's collapse.
        for (String host : new String[] { "127.0.0.1", MAPPED }) {
            try (ServerSocket ss = httpListener()) {
                String authority = host.contains(":") ? "[" + host + "]" : host;
                URL url = new URI("http://" + authority + ":" + ss.getLocalPort() + "/").toURL();
                try {
                    HttpURLConnection c = (HttpURLConnection) url.openConnection();
                    c.setConnectTimeout(5000);
                    c.setReadTimeout(5000);
                    c.connect();
                    int rc = c.getResponseCode();
                    check("HttpURLConnection to " + host, rc == 200, "rc=" + rc);
                    c.disconnect();
                } catch (Throwable t) {
                    check("HttpURLConnection to " + host, false, t.toString());
                }
            }
        }

        if (failures.isEmpty()) {
            System.out.println("ALL CHECKS PASSED");
        } else {
            System.out.println("FAILED CHECKS: " + failures);
            System.exit(1);
        }
    }
}
