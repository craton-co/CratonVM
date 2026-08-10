import com.sun.net.httpserver.HttpServer;

import java.net.InetSocketAddress;
import java.net.StandardSocketOptions;
import java.nio.channels.ServerSocketChannel;

/**
 * Decides whether CratonVM's wildcard→loopback rewrite in
 * `advertised_listener_host` is fixing a real problem or masking a different
 * one.
 *
 * `ssc_local_address` deliberately reports `127.0.0.1` for a wildcard-bound
 * `ServerSocketChannel`. The stated reason is that
 * `sun.net.httpserver.ServerImpl` binds a `ServerSocketChannel`, answers
 * `HttpServer.getAddress()` from it, and a caller that reconnects to a literal
 * `0.0.0.0` fails on Windows with WSAEADDRNOTAVAIL — the named casualty being
 * `RestClientBuilderIntegTests`.
 *
 * That reasoning only holds if HotSpot does something different here. So ask
 * HotSpot directly:
 *
 *   * if HotSpot's `getAddress()` also answers the wildcard, then real-world
 *     callers already cope with it, the rewrite is masking a *different*
 *     CratonVM defect in the reconnect path, and the accessor should tell the
 *     truth like HotSpot does;
 *   * if HotSpot answers a concrete address, the rewrite is emulating
 *     something real and must stay (or move to wherever HotSpot does it).
 *
 * The raw `ServerSocketChannel` row is printed alongside so the two layers can
 * be compared in one run — `HttpServer` is a consumer of exactly that API.
 */
public class HttpServerWildcardAddressProbe {

    public static void main(String[] args) throws Exception {
        // Layer 1: the channel itself, wildcard bind, ephemeral port.
        try (ServerSocketChannel ch = ServerSocketChannel.open()) {
            ch.setOption(StandardSocketOptions.SO_REUSEADDR, true);
            ch.bind(new InetSocketAddress("0.0.0.0", 0), 16);
            System.out.println("channel getLocalAddress()   = " + ch.getLocalAddress());
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
            System.out.println("HttpServer getAddress()     = " + addr);
            System.out.println("  .getAddress().isAnyLocal   = "
                    + (addr.getAddress() != null && addr.getAddress().isAnyLocalAddress()));
            System.out.println("  .getHostString()           = " + addr.getHostString());
            System.out.println("  .getPort()                 = " + addr.getPort());
        } finally {
            server.stop(0);
        }

        // Layer 3: the same question for an explicitly wildcard-bound server,
        // since `new InetSocketAddress(0)` is already the wildcard and a caller
        // may equally have written it out.
        HttpServer explicit = HttpServer.create(new InetSocketAddress("0.0.0.0", 0), 0);
        try {
            explicit.start();
            System.out.println("HttpServer explicit 0.0.0.0 = " + explicit.getAddress());
        } finally {
            explicit.stop(0);
        }
    }
}
