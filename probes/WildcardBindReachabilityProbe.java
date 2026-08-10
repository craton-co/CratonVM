import java.net.InetSocketAddress;
import java.net.StandardSocketOptions;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

/**
 * Answers ONE question about `ServerSocketChannel.bind("0.0.0.0", p)`:
 * is the socket genuinely bound to the wildcard, or only reported that way?
 *
 * CratonVM answers `getLocalAddress() == /127.0.0.1:p` where HotSpot answers
 * the wildcard. Those two readings have very different consequences — a server
 * really on loopback is unreachable from any other machine, while a wildcard
 * bind mis-*reported* as loopback is a fidelity bug with no reachability
 * impact — and `getLocalAddress()` is exactly the API under suspicion, so it
 * cannot be the thing that decides.
 *
 * So this asks the OS instead, three ways that do not go through the accessor:
 *
 *   1. Print the port and hold the socket open, so `netstat -ano` can be read
 *      from outside the process. A genuine wildcard listener shows
 *      `0.0.0.0:port`; a loopback one shows `127.0.0.1:port`.
 *   2. Connect to 127.0.0.1:port from inside the process. Succeeds either way —
 *      recorded so a failure here flags a broken run rather than proving
 *      anything on its own.
 *   3. Connect to a NON-loopback local address of this host. This is the
 *      discriminator: it succeeds against a wildcard listener and is refused
 *      against a loopback-only one.
 *
 * Usage: WildcardBindReachabilityProbe [port] [holdSeconds]
 */
public class WildcardBindReachabilityProbe {

    public static void main(String[] args) throws Exception {
        int port = (args.length > 0) ? Integer.parseInt(args[0]) : 0;
        int hold = (args.length > 1) ? Integer.parseInt(args[1]) : 20;

        ServerSocketChannel ch = ServerSocketChannel.open();
        try {
            try {
                ch.setOption(StandardSocketOptions.SO_REUSEADDR, true);
            } catch (Throwable t) {
                System.out.println("setOption threw = " + t.getClass().getName());
            }
            System.out.println("SO_REUSEADDR readback = " + ch.getOption(StandardSocketOptions.SO_REUSEADDR));

            ch.bind(new InetSocketAddress("0.0.0.0", port), 50);
            java.net.SocketAddress local = ch.getLocalAddress();
            int bound = ((InetSocketAddress) local).getPort();
            System.out.println("getLocalAddress()     = " + local);
            System.out.println("BOUND_PORT            = " + bound);
            System.out.flush();

            // (2) loopback — expected to work against either kind of listener.
            System.out.println("connect 127.0.0.1     = " + tryConnect("127.0.0.1", bound));

            // (3) the discriminator: a real, non-loopback address of this host.
            String lan = firstNonLoopbackIpv4();
            System.out.println("non-loopback addr     = " + lan);
            if (lan != null) {
                System.out.println("connect " + lan + " = " + tryConnect(lan, bound));
            }

            // (4) Connecting TO the wildcard. Windows resolves a connect to the
            // unspecified address as a connect to loopback, so this succeeds on
            // HotSpot — which is why HotSpot can afford to report the wildcard
            // from getLocalAddress()/HttpServer.getAddress() and still have
            // callers that reconnect to it work. CratonVM's accessor rewrites
            // the wildcard to loopback instead; if this row FAILS on CratonVM
            // and succeeds on HotSpot, the rewrite is compensating here, and
            // this is where the defect actually lives.
            System.out.println("connect 0.0.0.0       = " + tryConnect("0.0.0.0", bound));
            System.out.println("connect ::            = " + tryConnect("::", bound));

            System.out.println("holding " + hold + "s for netstat ...");
            System.out.flush();
            Thread.sleep(hold * 1000L);
        } finally {
            ch.close();
        }
    }

    static String tryConnect(String host, int port) {
        try (SocketChannel c = SocketChannel.open()) {
            c.socket().connect(new InetSocketAddress(host, port), 3000);
            return "OK";
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String firstNonLoopbackIpv4() {
        try {
            var ifs = java.net.NetworkInterface.getNetworkInterfaces();
            while (ifs != null && ifs.hasMoreElements()) {
                java.net.NetworkInterface ni = ifs.nextElement();
                if (!ni.isUp() || ni.isLoopback()) {
                    continue;
                }
                var addrs = ni.getInetAddresses();
                while (addrs.hasMoreElements()) {
                    java.net.InetAddress a = addrs.nextElement();
                    if (a instanceof java.net.Inet4Address && !a.isLoopbackAddress()) {
                        return a.getHostAddress();
                    }
                }
            }
        } catch (Throwable t) {
            return null;
        }
        return null;
    }
}
