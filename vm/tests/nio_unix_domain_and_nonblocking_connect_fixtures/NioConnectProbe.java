import java.net.InetSocketAddress;
import java.net.StandardProtocolFamily;
import java.net.StandardSocketOptions;
import java.net.UnixDomainSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.SelectionKey;
import java.nio.channels.Selector;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.charset.StandardCharsets;
import java.util.Set;

/** Output is compared with HotSpot's, line for line. */
public final class NioConnectProbe {
    static void say(String s) {
        System.out.println(s);
    }

    static volatile String serverSaid;

    public static void main(String[] args) throws Exception {
        // 1. A non-blocking connect is IN PROGRESS, and the selector reports OP_CONNECT.
        try (ServerSocketChannel ssc = ServerSocketChannel.open();
             SocketChannel sc = SocketChannel.open();
             Selector sel = Selector.open()) {
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            sc.configureBlocking(false);
            say("nb connect immediate=" + sc.connect(ssc.getLocalAddress()));
            say("pending=" + sc.isConnectionPending() + " connected=" + sc.isConnected());
            SelectionKey k = sc.register(sel, SelectionKey.OP_CONNECT);
            say("select=" + sel.select(5000) + " ready=" + k.readyOps());
            say("finishConnect=" + sc.finishConnect() + " connected=" + sc.isConnected());
        }
        // 2. A non-blocking connect to a closed port fails from finishConnect.
        int dead;
        try (ServerSocketChannel s = ServerSocketChannel.open()) {
            s.bind(new InetSocketAddress("127.0.0.1", 0));
            dead = ((InetSocketAddress) s.getLocalAddress()).getPort();
        }
        try (SocketChannel sc = SocketChannel.open(); Selector sel = Selector.open()) {
            sc.configureBlocking(false);
            boolean imm;
            try {
                imm = sc.connect(new InetSocketAddress("127.0.0.1", dead));
                say("refused connect immediate=" + imm);
                sc.register(sel, SelectionKey.OP_CONNECT);
                sel.select(5000);
                sc.finishConnect();
                say("refused: no exception");
            } catch (java.io.IOException e) {
                say("refused: " + e.getClass().getName() + ": " + e.getMessage());
            }
        }
        // 3. Unix-domain channels echo through a selector-less accept.
        Path dir = Files.createTempDirectory("nioprobe");
        Path sock = dir.resolve("s.sock");
        try (ServerSocketChannel srv = ServerSocketChannel.open(StandardProtocolFamily.UNIX)) {
            srv.bind(UnixDomainSocketAddress.of(sock));
            say("uds bound=" + (srv.getLocalAddress() instanceof UnixDomainSocketAddress)
                    + " name=" + ((UnixDomainSocketAddress) srv.getLocalAddress()).getPath().getFileName());
            Thread t = new Thread(() -> {
                try (SocketChannel c = srv.accept()) {
                    ByteBuffer b = ByteBuffer.allocate(64);
                    int n = c.read(b);
                    b.flip();
                    c.write(b);
                    serverSaid = "uds server read=" + n + " peerNamed="
                            + (((UnixDomainSocketAddress) c.getRemoteAddress()).getPath().toString().length() > 0);
                } catch (Exception e) {
                    serverSaid = "uds server failed " + e;
                }
            });
            t.start();
            try (SocketChannel c = SocketChannel.open(StandardProtocolFamily.UNIX)) {
                c.connect(UnixDomainSocketAddress.of(sock));
                c.write(ByteBuffer.wrap("hello-uds".getBytes(StandardCharsets.UTF_8)));
                ByteBuffer r = ByteBuffer.allocate(64);
                int n = c.read(r);
                r.flip();
                say("uds client echo=" + StandardCharsets.UTF_8.decode(r) + " n=" + n);
            }
            t.join();
            say(serverSaid);
        } finally {
            Files.deleteIfExists(sock);
            Files.deleteIfExists(dir);
        }
        // 4. Connecting a UNIX channel to a missing path.
        try (SocketChannel c = SocketChannel.open(StandardProtocolFamily.UNIX)) {
            c.connect(UnixDomainSocketAddress.of("/nonexistent-dir-nioprobe/x.sock"));
            say("missing path: connected?");
        } catch (java.io.IOException e) {
            say("missing path: " + e.getClass().getName() + ": " + e.getMessage());
        }
        // 5. Extended options on channels with no connection yet.
        try (SocketChannel sc = SocketChannel.open(); ServerSocketChannel ssc = ServerSocketChannel.open()) {
            Set<?> opts = sc.supportedOptions();
            say("sc supports TCP_NODELAY=" + opts.contains(StandardSocketOptions.TCP_NODELAY));
            for (java.net.SocketOption<?> o : sc.supportedOptions()) {
                String n = o.name();
                if (n.equals("TCP_QUICKACK") || n.equals("SO_INCOMING_NAPI_ID")) {
                    say("sc " + n + " default=" + sc.getOption(o));
                }
            }
            for (java.net.SocketOption<?> o : ssc.supportedOptions()) {
                String n = o.name();
                if (n.equals("TCP_QUICKACK") || n.equals("SO_INCOMING_NAPI_ID")) {
                    say("ssc " + n + " default=" + ssc.getOption(o));
                }
            }
        }
        say("NIO-CONNECT-PROBE-DONE");
    }
}
