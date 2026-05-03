import java.net.*;
import java.nio.*;
import java.nio.channels.*;
import java.util.*;
public class SelectorProbe {
    public static void main(String[] a) throws Exception {
        ServerSocketChannel ssc = ServerSocketChannel.open();
        ssc.configureBlocking(false);
        ssc.socket().bind(new InetSocketAddress(0));
        int port = ssc.socket().getLocalPort();
        Selector sel = Selector.open();
        ssc.register(sel, SelectionKey.OP_ACCEPT);
        System.out.println("server.port=" + port);

        SocketChannel client = SocketChannel.open(new InetSocketAddress("127.0.0.1", port));
        client.configureBlocking(false);

        long deadline = System.currentTimeMillis() + 5000;
        SocketChannel accepted = null;
        while (accepted == null && System.currentTimeMillis() < deadline) {
            sel.select(500);
            for (Iterator<SelectionKey> it = sel.selectedKeys().iterator(); it.hasNext();) {
                SelectionKey k = it.next();
                it.remove();
                if (k.isAcceptable()) {
                    accepted = ((ServerSocketChannel) k.channel()).accept();
                    System.out.println("server.accepted=true");
                }
            }
        }
        if (accepted == null) { System.err.println("FAIL accept"); System.exit(1); }

        client.write(ByteBuffer.wrap(new byte[]{42}));
        ByteBuffer buf = ByteBuffer.allocate(1);
        int total = 0;
        deadline = System.currentTimeMillis() + 2000;
        while (total < 1 && System.currentTimeMillis() < deadline) {
            int n = accepted.read(buf);
            if (n > 0) total += n;
            else if (n < 0) break;
        }
        buf.flip();
        System.out.println("server.recv=" + buf.get(0));

        sel.close(); client.close(); accepted.close(); ssc.close();
        System.out.println("OK");
    }
}
