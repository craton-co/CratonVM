import java.net.InetSocketAddress;
import java.net.SocketOption;
import java.net.StandardProtocolFamily;
import java.net.StandardSocketOptions;
import java.nio.channels.DatagramChannel;
import java.nio.channels.NetworkChannel;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.spi.SelectorProvider;
import java.util.Set;
import java.util.TreeSet;

/**
 * The two coupled datagram gaps from
 * docs/known-issues/netty/nio-channels-are-abstract-classed-*.md:
 *   1. SelectorProvider.openDatagramChannel() (no-arg) is not bridged.
 *   2. getOption / setOption / supportedOptions are missing on the channel.
 * Plus the same generic-SocketOption surface on a UNIX-family server channel.
 */
public class DatagramChannelOptionProbe {
    static void row(String name, Callable c) {
        String out;
        try {
            out = "OK   " + c.call();
        } catch (Throwable t) {
            String m = t.getMessage();
            if (m != null && m.length() > 100) m = m.substring(0, 100);
            out = "FAIL " + t.getClass().getName() + ": " + m;
        }
        System.out.println(String.format("%-30s %s", name, out));
    }

    interface Callable { Object call() throws Throwable; }

    public static void main(String[] args) throws Throwable {
        SelectorProvider sp = SelectorProvider.provider();

        row("provider.class", () -> sp.getClass().getName());
        row("DatagramChannel.open.class", () -> DatagramChannel.open().getClass().getName());

        row("sp.openDatagramChannel()", () -> {
            DatagramChannel a = sp.openDatagramChannel();
            return "class=" + a.getClass().getName() + " isOpen=" + a.isOpen()
                    + " socketClosed=" + a.socket().isClosed();
        });
        row("sp.openDatagramChannel(INET)", () -> {
            DatagramChannel b = sp.openDatagramChannel(StandardProtocolFamily.INET);
            return "class=" + b.getClass().getName() + " isOpen=" + b.isOpen();
        });

        final DatagramChannel dc = sp.openDatagramChannel();
        row("dc.supportedOptions", () -> {
            Set<String> names = new TreeSet<>();
            for (SocketOption<?> o : dc.supportedOptions()) names.add(o.name());
            return names;
        });
        row("dc.supports TCP_NODELAY?", () ->
                dc.supportedOptions().contains(StandardSocketOptions.TCP_NODELAY));
        row("dc.getOption(SO_REUSEADDR)", () -> {
            Object v = dc.getOption(StandardSocketOptions.SO_REUSEADDR);
            return v + " (" + (v == null ? "null" : v.getClass().getSimpleName()) + ")";
        });
        row("dc SO_REUSEADDR roundtrip", () -> {
            boolean v1 = dc.getOption(StandardSocketOptions.SO_REUSEADDR);
            dc.setOption(StandardSocketOptions.SO_REUSEADDR, !v1);
            boolean v2 = dc.getOption(StandardSocketOptions.SO_REUSEADDR);
            return "before=" + v1 + " after=" + v2 + " flipped=" + (v1 != v2);
        });
        row("dc SO_BROADCAST roundtrip", () -> {
            dc.setOption(StandardSocketOptions.SO_BROADCAST, true);
            return dc.getOption(StandardSocketOptions.SO_BROADCAST);
        });
        row("dc SO_RCVBUF roundtrip", () -> {
            dc.setOption(StandardSocketOptions.SO_RCVBUF, 65536);
            return dc.getOption(StandardSocketOptions.SO_RCVBUF);
        });
        row("dc SO_SNDBUF roundtrip", () -> {
            dc.setOption(StandardSocketOptions.SO_SNDBUF, 65536);
            return dc.getOption(StandardSocketOptions.SO_SNDBUF);
        });
        row("dc IP_TOS roundtrip", () -> {
            dc.setOption(StandardSocketOptions.IP_TOS, 8);
            return dc.getOption(StandardSocketOptions.IP_TOS);
        });
        row("dc IP_MULTICAST_TTL", () -> {
            dc.setOption(StandardSocketOptions.IP_MULTICAST_TTL, 3);
            return dc.getOption(StandardSocketOptions.IP_MULTICAST_TTL);
        });
        row("dc IP_MULTICAST_LOOP", () ->
                dc.getOption(StandardSocketOptions.IP_MULTICAST_LOOP));
        row("dc IP_MULTICAST_IF", () ->
                String.valueOf(dc.getOption(StandardSocketOptions.IP_MULTICAST_IF)));
        row("dc setOption returns this", () ->
                dc.setOption(StandardSocketOptions.SO_BROADCAST, true) == dc);
        row("dc as NetworkChannel", () -> {
            NetworkChannel nc = dc;
            return nc.getOption(StandardSocketOptions.SO_REUSEADDR);
        });

        // The DatagramSocket adaptor surface netty's config reaches.
        row("dc.socket().isClosed", () -> dc.socket().isClosed());
        row("dc.socket().setBroadcast", () -> { dc.socket().setBroadcast(true); return "set"; });
        row("dc.socket().getBroadcast", () -> dc.socket().getBroadcast());
        row("dc.bind+getLocalAddress", () -> {
            dc.bind(new InetSocketAddress(0));
            return dc.getLocalAddress() != null;
        });
        row("dc.socket().getLocalAddr", () ->
                String.valueOf(dc.socket().getLocalSocketAddress()));

        // UNIX-family server channel — the "probably the same family" residual.
        row("ssc(UNIX) supportedOpts", () -> {
            ServerSocketChannel u = sp.openServerSocketChannel(StandardProtocolFamily.UNIX);
            Set<String> names = new TreeSet<>();
            for (SocketOption<?> o : u.supportedOptions()) names.add(o.name());
            return names;
        });
        row("ssc(UNIX) REUSEADDR trip", () -> {
            ServerSocketChannel u = sp.openServerSocketChannel(StandardProtocolFamily.UNIX);
            boolean v1 = u.getOption(StandardSocketOptions.SO_REUSEADDR);
            u.setOption(StandardSocketOptions.SO_REUSEADDR, !v1);
            boolean v2 = u.getOption(StandardSocketOptions.SO_REUSEADDR);
            return "before=" + v1 + " after=" + v2 + " flipped=" + (v1 != v2);
        });
        row("ssc(UNIX) supports MCAST_IF", () -> {
            ServerSocketChannel u = sp.openServerSocketChannel(StandardProtocolFamily.UNIX);
            return u.supportedOptions().contains(StandardSocketOptions.IP_MULTICAST_IF);
        });

        System.out.println("PROBE-END");
    }
}
