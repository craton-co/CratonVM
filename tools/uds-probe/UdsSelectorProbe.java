import java.io.File;
import java.net.SocketAddress;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.util.Iterator;

/**
 * Mimics Tomcat's NioEndpoint over a Unix domain socket: a blocking acceptor
 * thread hands each accepted channel to a Selector-driven poller, which reads
 * the request and writes the response.
 */
public class UdsSelectorProbe {
    public static void main(String[] args) throws Exception {
        File tmp = File.createTempFile("uds-sel-probe-", ".sock");
        String path = tmp.getAbsolutePath();
        if (!tmp.delete()) {
            System.out.println("FAIL could not delete " + path);
            return;
        }
        SocketAddress sa = UnixDomainSocketAddress.of(path);
        ServerSocketChannel server = ServerSocketChannel.open(StandardProtocolFamily.UNIX);
        server.bind(sa, 100);
        server.configureBlocking(true); // mimic NioEndpoint
        System.out.println("OK   bound " + server.getLocalAddress());

        Selector selector = Selector.open();
        System.out.println("OK   selector " + selector.getClass().getName());

        Thread acceptor = new Thread(() -> {
            try {
                SocketChannel ch = server.accept();
                System.out.println("OK   accepted " + ch);
                ch.configureBlocking(false);
                SelectionKey key = ch.register(selector, SelectionKey.OP_READ);
                System.out.println("OK   registered key=" + key + " valid=" + key.isValid());
                selector.wakeup();
            } catch (Throwable t) {
                System.out.println("FAIL acceptor -> " + t);
                t.printStackTrace(System.out);
            }
        }, "acceptor");
        acceptor.setDaemon(true);
        acceptor.start();

        Thread poller = new Thread(() -> {
            long deadline = System.currentTimeMillis() + 20000;
            try {
                while (System.currentTimeMillis() < deadline) {
                    int n = selector.select(500);
                    if (n == 0) {
                        continue;
                    }
                    Iterator<SelectionKey> it = selector.selectedKeys().iterator();
                    while (it.hasNext()) {
                        SelectionKey k = it.next();
                        it.remove();
                        if (!k.isReadable()) {
                            continue;
                        }
                        SocketChannel ch = (SocketChannel) k.channel();
                        ByteBuffer in = ByteBuffer.allocate(256);
                        int r = ch.read(in);
                        System.out.println("OK   poller read n=" + r + " text="
                                + new String(in.array(), 0, Math.max(r, 0)).trim());
                        ch.write(ByteBuffer.wrap("HTTP/1.1 200 OK\r\n\r\n".getBytes()));
                        System.out.println("OK   poller wrote response");
                        return;
                    }
                }
                System.out.println("FAIL poller timed out with no readable key");
            } catch (Throwable t) {
                System.out.println("FAIL poller -> " + t);
                t.printStackTrace(System.out);
            }
        }, "poller");
        poller.setDaemon(true);
        poller.start();

        try (SocketChannel client = SocketChannel.open(StandardProtocolFamily.UNIX)) {
            client.connect(sa);
            System.out.println("OK   client connected");
            client.write(ByteBuffer.wrap("OPTIONS * HTTP/1.0\r\n\r\n".getBytes()));
            ByteBuffer resp = ByteBuffer.allocate(256);
            long deadline = System.currentTimeMillis() + 20000;
            int n = 0;
            while (n <= 0 && System.currentTimeMillis() < deadline) {
                n = client.read(resp);
                if (n == 0) {
                    Thread.sleep(50);
                }
            }
            System.out.println((n > 0 ? "OK   " : "FAIL ") + "client got n=" + n + " text="
                    + new String(resp.array(), 0, Math.max(n, 0)).trim());
        } finally {
            poller.join(25000);
            server.close();
            selector.close();
            new File(path).delete();
        }
        System.out.println("DONE");
        System.exit(0);
    }
}
