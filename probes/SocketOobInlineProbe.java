import java.net.InetSocketAddress;
import java.net.Socket;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.channels.spi.SelectorProvider;

/**
 * Isolates `SocketAdaptor.getOOBInline()`, which passes
 * sun.nio.ch.ExtendedSocketOption.SO_OOBINLINE — a SocketOption with no `name`
 * FIELD — so the option-name reader has to fall back to `name()`. Kept separate
 * from AdaptorAudit because the datagram half of that probe cannot run at all on
 * a binary without the openDatagramChannel() bridge.
 */
public class SocketOobInlineProbe {
    public static void main(String[] args) throws Throwable {
        SelectorProvider sp = SelectorProvider.provider();
        ServerSocketChannel ssc = sp.openServerSocketChannel();
        ssc.bind(new InetSocketAddress(0));
        SocketChannel sc = sp.openSocketChannel();
        sc.connect(new InetSocketAddress("127.0.0.1", ssc.socket().getLocalPort()));
        Socket s = sc.socket();
        try {
            s.setOOBInline(true);
            System.out.println("setOOBInline OK");
        } catch (Throwable t) {
            System.out.println("setOOBInline FAIL " + t);
        }
        try {
            System.out.println("getOOBInline OK " + s.getOOBInline());
        } catch (Throwable t) {
            System.out.println("getOOBInline FAIL " + t);
        }
        sc.close();
        ssc.close();
        System.out.println("OOB-END");
    }
}
