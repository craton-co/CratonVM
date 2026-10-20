import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.StandardProtocolFamily;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;

/**
 * What a wildcard-bound TCP listener REPORTS and who can reach it, for every
 * way of binding one. HotSpot binds the wildcard as dual-stack AF_INET6 when
 * IPv6 is available (ServerSocketChannel / ServerSocket), so the channel's
 * local address is `/[0:0:0:0:0:0:0:0]` and a client may connect over either
 * loopback; an explicit `StandardProtocolFamily.INET` channel stays IPv4.
 * Ports are erased; the rest is printed verbatim.
 * Page: wildcard-bind-is-ipv4-and-windows-has-no-real-channel-layer-FIXED-20261013.
 */
public class Nio21WildcardV6 {
    static String norm(Object o) {
        String s = String.valueOf(o);
        int c = s.lastIndexOf(':');
        return c < 0 ? s : s.substring(0, c + 1) + "<p>";
    }

    static void reach(String label, int port) {
        for (String h : new String[] {"127.0.0.1", "::1"}) {
            try (Socket s = new Socket()) {
                s.connect(new InetSocketAddress(InetAddress.getByName(h), port), 2000);
                System.out.println(label + " connect " + h + " = ok");
            } catch (Exception e) {
                System.out.println(label + " connect " + h + " = " + e.getClass().getName());
            }
        }
    }

    public static void main(String[] a) throws Exception {
        try (ServerSocketChannel c = ServerSocketChannel.open()) {
            c.bind(new InetSocketAddress(0));
            System.out.println("ssc bind(0)       local = " + norm(c.getLocalAddress()));
            reach("ssc bind(0)      ", ((InetSocketAddress) c.getLocalAddress()).getPort());
        }
        try (ServerSocketChannel c = ServerSocketChannel.open()) {
            c.bind(new InetSocketAddress("0.0.0.0", 0));
            System.out.println("ssc bind(0.0.0.0) local = " + norm(c.getLocalAddress()));
        }
        try (ServerSocketChannel c = ServerSocketChannel.open()) {
            c.bind(null);
            System.out.println("ssc bind(null)    local = " + norm(c.getLocalAddress()));
        }
        try (ServerSocketChannel c = ServerSocketChannel.open(StandardProtocolFamily.INET)) {
            c.bind(new InetSocketAddress(0));
            System.out.println("ssc INET family   local = " + norm(c.getLocalAddress()));
            reach("ssc INET family  ", ((InetSocketAddress) c.getLocalAddress()).getPort());
        }
        try (ServerSocketChannel c = ServerSocketChannel.open()) {
            c.bind(new InetSocketAddress("127.0.0.1", 0));
            System.out.println("ssc bind(127.0.0.1) local = " + norm(c.getLocalAddress()));
        }
        try (ServerSocket s = new ServerSocket(0)) {
            System.out.println("ServerSocket(0)   inet  = " + s.getInetAddress());
            System.out.println("ServerSocket(0)   local = " + norm(s.getLocalSocketAddress()));
            reach("ServerSocket(0)  ", s.getLocalPort());
        }
        try (ServerSocket s = new ServerSocket()) {
            s.bind(new InetSocketAddress("0.0.0.0", 0));
            System.out.println("ServerSocket bind 0.0.0.0 local = " + norm(s.getLocalSocketAddress()));
        }
        try (ServerSocketChannel c = ServerSocketChannel.open()) {
            c.bind(new InetSocketAddress(0));
            int port = ((InetSocketAddress) c.getLocalAddress()).getPort();
            try (SocketChannel sc = SocketChannel.open(new InetSocketAddress("127.0.0.1", port));
                 SocketChannel acc = c.accept()) {
                System.out.println("accepted local  = " + norm(acc.getLocalAddress()));
                System.out.println("accepted remote = " + norm(acc.getRemoteAddress()));
                System.out.println("client local    = " + norm(sc.getLocalAddress()));
            }
        }
    }
}
