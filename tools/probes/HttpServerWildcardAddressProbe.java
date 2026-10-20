import com.sun.net.httpserver.HttpServer;

import java.net.InetSocketAddress;
import java.net.StandardSocketOptions;
import java.nio.channels.ServerSocketChannel;

/**
 * Asks HotSpot what a wildcard-bound listener should REPORT, at two layers.
 *
 * HISTORY, because the answer already changed the code once. CratonVM used to
 * rewrite a wildcard local address to loopback (`0.0.0.0` -> `127.0.0.1`,
 * `::` -> `::1`) in `advertised_listener_host`, on the premise that Windows
 * rejects a connect to an unspecified address with WSAEADDRNOTAVAIL and that
 * `sun.net.httpserver.ServerImpl.getAddress()` therefore had to publish
 * something reconnectable. THIS PROBE REFUTED BOTH HALVES on 2026-08-10:
 * HotSpot itself answers the wildcard with `isAnyLocalAddress() == true`, and
 * connecting to `0.0.0.0` works on both VMs. The rewrite was REMOVED; the
 * measurement is quoted in `native-io/src/socket_channel.rs` above
 * `advertised_listener_host`, which is now the identity function.
 *
 * So this probe no longer asks whether the rewrite is justified -- there is no
 * rewrite. It is now the REGRESSION for its absence, plus the open question
 * that removal did not settle: HotSpot binds the wildcard as a dual-stack
 * IPv6 socket and reports `[0:0:0:0:0:0:0:0]`, while CratonVM's HttpServer
 * layer reports the v4 `0.0.0.0`. Both satisfy `isAnyLocalAddress()`, but a
 * caller that connects to `::` reaches a v4-only listener on one and a
 * dual-stack one on the other.
 *
 * The raw `ServerSocketChannel` row is printed alongside the `HttpServer` rows
 * so the two layers can be compared in one run -- `HttpServer` is a consumer
 * of exactly that API, and on the real-JDK arms the two DISAGREE: the channel
 * matches HotSpot and the HttpServer above it does not.
 *
 * EVERY address row goes through {@link #norm}: see its comment for why this
 * probe could not be scored at all before 2026-09-02.
 */
public class HttpServerWildcardAddressProbe {

    public static void main(String[] args) throws Exception {
        // Layer 1: the channel itself, wildcard bind, ephemeral port.
        try (ServerSocketChannel ch = ServerSocketChannel.open()) {
            ch.setOption(StandardSocketOptions.SO_REUSEADDR, true);
            ch.bind(new InetSocketAddress("0.0.0.0", 0), 16);
            System.out.println("channel getLocalAddress()   = " + norm(ch.getLocalAddress()));
            System.out.println("channel SO_REUSEADDR        = "
                    + ch.getOption(StandardSocketOptions.SO_REUSEADDR));
        }

        // Layer 2: HttpServer, which binds a ServerSocketChannel internally and
        // republishes the address through getAddress(). This is the consumer
        // the rewrite was added for.
        HttpServer server = HttpServer.create(new InetSocketAddress(0), 0);
        try {
            server.start();
            InetSocketAddress addr = server.getAddress();
            System.out.println("HttpServer getAddress()     = " + norm(addr));
            System.out.println("  .getAddress().isAnyLocal   = "
                    + (addr.getAddress() != null && addr.getAddress().isAnyLocalAddress()));
            System.out.println("  .getHostString()           = " + addr.getHostString());
            // The NUMBER is noise; that a port was bound at all is not.
            System.out.println("  .getPort() bound           = " + (addr.getPort() > 0));
        } finally {
            server.stop(0);
        }

        // Layer 3: the same question for an explicitly wildcard-bound server,
        // since `new InetSocketAddress(0)` is already the wildcard and a caller
        // may equally have written it out.
        HttpServer explicit = HttpServer.create(new InetSocketAddress("0.0.0.0", 0), 0);
        try {
            explicit.start();
            System.out.println("HttpServer explicit 0.0.0.0 = " + norm(explicit.getAddress()));
        } finally {
            explicit.stop(0);
        }
    }

    /**
     * Erases the ephemeral port from an address rendering, and NOTHING else.
     *
     * Every bind in this probe asks for port 0, so the kernel picks a
     * different number on every run of either VM. A raw diff of this probe's
     * output therefore reports differing rows on two runs of the SAME binary,
     * which is how a probe comes to count noise as signal — the state that
     * left `W7-24` unscoreable until now.
     *
     * Only the trailing `:<digits>` goes, so the address itself survives
     * intact. That matters here: HotSpot answers the IPv6 wildcard
     * `/[0:0:0:0:0:0:0:0]:PORT`, whose ADDRESS contains colon-digit pairs a
     * careless rewrite would eat, destroying exactly the family difference
     * this probe exists to see.
     *
     * The fact that a port was bound at all is signal and is preserved
     * separately, as a `bound` boolean — a probe that hid a failure to bind
     * behind its own normalisation would be worse than the unscoreable one.
     */
    static String norm(Object o) {
        String s = String.valueOf(o);
        int c = s.lastIndexOf(':');
        if (c < 0 || c == s.length() - 1) {
            return s;
        }
        String tail = s.substring(c + 1);
        for (int i = 0; i < tail.length(); i++) {
            if (!Character.isDigit(tail.charAt(i))) {
                return s;
            }
        }
        return s.substring(0, c + 1) + "<ephemeral>";
    }
}
