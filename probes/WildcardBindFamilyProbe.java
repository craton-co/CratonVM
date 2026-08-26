import java.io.IOException;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.StandardProtocolFamily;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

/**
 * Which family does a WILDCARD server bind land on, and can each loopback
 * family reach it?
 *
 * `sun.nio.ch.Net.serverSocket` opens AF_INET6 with `IPV6_V6ONLY` cleared
 * whenever IPv6 is available and no explicit `StandardProtocolFamily.INET` was
 * given, so on HotSpot a wildcard bind reports `0:0:0:0:0:0:0:0` and accepts
 * BOTH `127.0.0.1` and `::1`. CratonVM bound `0.0.0.0` — a real AF_INET
 * listener — so every `::1` client got `Connection refused`, which is how all
 * 48 parameterisations of `SSLEngineTest.testMutualAuthDiffCerts` failed at
 * `assertTrue(ccf.awaitUninterruptibly().isSuccess())` in four netty SSL
 * classes at once. See
 * `fixed-suite-bugs/netty/ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`.
 *
 * Every row is a HotSpot-parity assertion, including the two NEGATIVE ones:
 * `open(INET)` must stay v4-only, and an explicitly v4 bind must stay v4-only.
 * A fix that made everything dual-stack would pass the positive rows and fail
 * those two.
 *
 * usage: WildcardBindFamilyProbe        (prints @@ROW lines; exit 1 on any FAIL)
 */
public final class WildcardBindFamilyProbe {
    private WildcardBindFamilyProbe() {}

    private static int failures;

    public static void main(String[] args) throws Exception {
        System.out.println("@@ENV preferIPv4Stack=" + System.getProperty("java.net.preferIPv4Stack")
                + " loopback=" + InetAddress.getLoopbackAddress());

        // ServerSocketChannel.open() — no family. Dual-stack on HotSpot.
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress(0));
            check("ssc.open()", ssc.getLocalAddress(), true, true);
        }

        // ServerSocketChannel.open(INET6) — also dual-stack on HotSpot: the JDK
        // clears IPV6_V6ONLY on every AF_INET6 socket it opens.
        try (ServerSocketChannel ssc = ServerSocketChannel.open(StandardProtocolFamily.INET6)) {
            ssc.bind(new InetSocketAddress(0));
            check("ssc.open(INET6)", ssc.getLocalAddress(), true, true);
        }

        // ServerSocketChannel.open(INET) — AF_INET, and must STAY v4-only.
        try (ServerSocketChannel ssc = ServerSocketChannel.open(StandardProtocolFamily.INET)) {
            ssc.bind(new InetSocketAddress(0));
            check("ssc.open(INET)", ssc.getLocalAddress(), true, false);
        }

        // An EXPLICIT v4 bind address must stay v4-only whatever the family.
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0));
            check("ssc.bind(127.0.0.1)", ssc.getLocalAddress(), true, false);
        }

        // ServerSocketChannel.bind(null) — the specified "pick an address"
        // form, which is a wildcard bind too.
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(null);
            check("ssc.bind(null)", ssc.getLocalAddress(), true, true);
        }

        // java.net.ServerSocket, both wildcard spellings.
        try (ServerSocket ss = new ServerSocket(0)) {
            check("new ServerSocket(0)",
                    new InetSocketAddress(ss.getInetAddress(), ss.getLocalPort()), true, true);
        }
        try (ServerSocket ss = new ServerSocket()) {
            ss.bind(new InetSocketAddress(0));
            check("ServerSocket().bind(0)",
                    new InetSocketAddress(ss.getInetAddress(), ss.getLocalPort()), true, true);
        }

        System.out.println("@@PROBE failures=" + failures);
        if (failures != 0) {
            System.exit(1);
        }
    }

    /**
     * Report the bound address, then try each loopback family against it and
     * compare with what HotSpot does.
     */
    private static void check(String label, java.net.SocketAddress local,
            boolean expectV4Reachable, boolean expectV6Reachable) {
        InetSocketAddress isa = (InetSocketAddress) local;
        int port = isa.getPort();
        boolean v4 = reachable("127.0.0.1", port);
        boolean v6 = reachable("::1", port);
        boolean ok = v4 == expectV4Reachable && v6 == expectV6Reachable;
        if (!ok) {
            failures++;
        }
        System.out.println("@@ROW " + (ok ? "PASS" : "FAIL") + " " + label
                + " bound=" + isa.getAddress()
                + " boundClass=" + isa.getAddress().getClass().getSimpleName()
                + " v4Reachable=" + v4 + "(want " + expectV4Reachable + ")"
                + " v6Reachable=" + v6 + "(want " + expectV6Reachable + ")");
    }

    private static boolean reachable(String host, int port) {
        // Two client surfaces, because they are two different code paths in the
        // VM and a fix can land in one of them: SocketChannel first, then the
        // plain java.net.Socket, and both have to agree.
        boolean viaChannel;
        try (SocketChannel sc = SocketChannel.open()) {
            viaChannel = sc.connect(new InetSocketAddress(InetAddress.getByName(host), port));
        } catch (IOException e) {
            viaChannel = false;
        }
        boolean viaSocket;
        try (Socket s = new Socket()) {
            s.connect(new InetSocketAddress(InetAddress.getByName(host), port), 5000);
            viaSocket = s.isConnected();
        } catch (IOException e) {
            viaSocket = false;
        }
        if (viaChannel != viaSocket) {
            System.out.println("@@SPLIT " + host + ":" + port
                    + " SocketChannel=" + viaChannel + " Socket=" + viaSocket);
        }
        return viaChannel || viaSocket;
    }

    @SuppressWarnings("unused")
    private static boolean isV4(InetAddress a) {
        return a instanceof Inet4Address;
    }
}
