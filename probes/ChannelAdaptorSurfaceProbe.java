import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.net.DatagramSocket;
import java.nio.channels.DatagramChannel;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.channels.spi.SelectorProvider;

/**
 * The `*Adaptor` surface the nio-channels doc is about: `channel.socket()`
 * hands back REAL JDK bytecode whose bodies call methods declared on the
 * `sun.nio.ch.*Impl` class, which CratonVM's abstract-classed channel does not
 * have. Each hole is per-method, so this walks the whole adaptor surface at
 * once instead of finding them one application at a time.
 */
public class ChannelAdaptorSurfaceProbe {
    static void row(String name, Callable c) {
        String out;
        try {
            out = "OK   " + c.call();
        } catch (Throwable t) {
            String m = t.getMessage();
            if (m != null && m.length() > 80) m = m.substring(0, 80);
            out = "FAIL " + t.getClass().getName() + ": " + m;
        }
        System.out.println(String.format("%-34s %s", name, out));
    }

    interface Callable { Object call() throws Throwable; }

    public static void main(String[] args) throws Throwable {
        SelectorProvider sp = SelectorProvider.provider();

        final DatagramChannel dc = sp.openDatagramChannel();
        dc.bind(new InetSocketAddress(0));
        final DatagramSocket ds = dc.socket();
        row("ds.getClass", () -> ds.getClass().getName());
        row("ds.isBound", () -> ds.isBound());
        row("ds.isClosed", () -> ds.isClosed());
        row("ds.isConnected", () -> ds.isConnected());
        row("ds.getLocalPort>0", () -> ds.getLocalPort() > 0);
        row("ds.getLocalAddress!=null", () -> ds.getLocalAddress() != null);
        row("ds.getLocalSocketAddress", () -> ds.getLocalSocketAddress() != null);
        row("ds.getRemoteSocketAddress", () -> String.valueOf(ds.getRemoteSocketAddress()));
        row("ds.getPort", () -> ds.getPort());
        row("ds.setSoTimeout/get", () -> { ds.setSoTimeout(1234); return ds.getSoTimeout(); });
        row("ds.setBroadcast/get", () -> { ds.setBroadcast(true); return ds.getBroadcast(); });
        row("ds.setReuseAddress/get", () -> { ds.setReuseAddress(true); return ds.getReuseAddress(); });
        row("ds.setReceiveBufSize/get", () -> { ds.setReceiveBufferSize(65536); return ds.getReceiveBufferSize() > 0; });
        row("ds.setSendBufSize/get", () -> { ds.setSendBufferSize(65536); return ds.getSendBufferSize() > 0; });
        row("ds.setTrafficClass/get", () -> { ds.setTrafficClass(8); return ds.getTrafficClass(); });
        row("ds.getChannel==dc", () -> ds.getChannel() == dc);
        row("ds.supportedOptions.size", () -> ds.supportedOptions().size());
        row("ds.getOption(SO_RCVBUF)>0", () -> ds.getOption(java.net.StandardSocketOptions.SO_RCVBUF) > 0);
        row("ds.setOption(SO_BROADCAST)", () -> {
            ds.setOption(java.net.StandardSocketOptions.SO_BROADCAST, true);
            return ds.getOption(java.net.StandardSocketOptions.SO_BROADCAST);
        });

        final ServerSocketChannel ssc = sp.openServerSocketChannel();
        ssc.bind(new InetSocketAddress(0));
        final ServerSocket ss = ssc.socket();
        row("ss.getClass", () -> ss.getClass().getName());
        row("ss.isBound", () -> ss.isBound());
        row("ss.isClosed", () -> ss.isClosed());
        row("ss.getLocalPort>0", () -> ss.getLocalPort() > 0);
        row("ss.getInetAddress!=null", () -> ss.getInetAddress() != null);
        row("ss.getLocalSocketAddress", () -> ss.getLocalSocketAddress() != null);
        row("ss.setSoTimeout/get", () -> { ss.setSoTimeout(1234); return ss.getSoTimeout(); });
        row("ss.setReuseAddress/get", () -> { ss.setReuseAddress(true); return ss.getReuseAddress(); });
        row("ss.setReceiveBufSize/get", () -> { ss.setReceiveBufferSize(65536); return ss.getReceiveBufferSize() > 0; });
        row("ss.getChannel==ssc", () -> ss.getChannel() == ssc);
        row("ss.supportedOptions.size", () -> ss.supportedOptions().size());

        final SocketChannel sc = sp.openSocketChannel();
        sc.connect(new InetSocketAddress("127.0.0.1", ssc.socket().getLocalPort()));
        final Socket s = sc.socket();
        row("s.getClass", () -> s.getClass().getName());
        row("s.isConnected", () -> s.isConnected());
        row("s.isBound", () -> s.isBound());
        row("s.isClosed", () -> s.isClosed());
        row("s.getPort>0", () -> s.getPort() > 0);
        row("s.getLocalPort>0", () -> s.getLocalPort() > 0);
        row("s.getInetAddress!=null", () -> s.getInetAddress() != null);
        row("s.getRemoteSocketAddress", () -> s.getRemoteSocketAddress() != null);
        row("s.getLocalSocketAddress", () -> s.getLocalSocketAddress() != null);
        row("s.setTcpNoDelay/get", () -> { s.setTcpNoDelay(true); return s.getTcpNoDelay(); });
        row("s.setKeepAlive/get", () -> { s.setKeepAlive(true); return s.getKeepAlive(); });
        row("s.setSoLinger/get", () -> { s.setSoLinger(true, 5); return s.getSoLinger(); });
        row("s.setSoTimeout/get", () -> { s.setSoTimeout(1234); return s.getSoTimeout(); });
        row("s.setSendBufSize/get", () -> { s.setSendBufferSize(65536); return s.getSendBufferSize() > 0; });
        row("s.setRecvBufSize/get", () -> { s.setReceiveBufferSize(65536); return s.getReceiveBufferSize() > 0; });
        row("s.setTrafficClass/get", () -> { s.setTrafficClass(8); return s.getTrafficClass(); });
        row("s.setOOBInline/get", () -> { s.setOOBInline(true); return s.getOOBInline(); });
        row("s.getChannel==sc", () -> s.getChannel() == sc);
        row("s.supportedOptions.size", () -> s.supportedOptions().size());

        sc.close(); ssc.close(); dc.close();
        System.out.println("ADAPTOR-END");
    }
}
