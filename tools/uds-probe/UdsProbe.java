import java.io.File;
import java.lang.reflect.Method;
import java.net.SocketAddress;
import java.net.StandardProtocolFamily;
import java.nio.ByteBuffer;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Path;

/** Probe the JDK-bytecode prerequisites for Unix-domain-socket support. */
public class UdsProbe {
    static void step(String name, Runnable r) {
        try {
            r.run();
            System.out.println("OK   " + name);
        } catch (Throwable t) {
            System.out.println("FAIL " + name + " -> " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("java.version=" + System.getProperty("java.version"));

        step("Path.of", () -> {
            Path p = Path.of("C:\\tmp\\x.sock");
            System.out.println("     path=" + p);
        });
        step("FileSystems.getDefault identity", () -> {
            Path p = Path.of("C:\\tmp\\x.sock");
            FileSystem fs = p.getFileSystem();
            System.out.println("     fs=" + fs.getClass().getName()
                    + " sameAsDefault=" + (fs == FileSystems.getDefault()));
        });
        step("getModule identity", () -> {
            Path p = Path.of("C:\\tmp\\x.sock");
            FileSystem fs = p.getFileSystem();
            Object m1 = fs.getClass().getModule();
            Object m2 = Object.class.getModule();
            System.out.println("     fsModule=" + m1 + " objModule=" + m2 + " same=" + (m1 == m2));
        });
        step("UnixDomainSocketAddress.of(String)", () -> {
            try {
                Class<?> c = Class.forName("java.net.UnixDomainSocketAddress");
                Method of = c.getMethod("of", String.class);
                Object a = of.invoke(null, "C:\\tmp\\x.sock");
                System.out.println("     addr=" + a + " class=" + a.getClass().getName());
                Method gp = c.getMethod("getPath");
                System.out.println("     getPath=" + gp.invoke(a));
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        });
        step("ServerSocketChannel.open(UNIX)", () -> {
            try (ServerSocketChannel ch = ServerSocketChannel.open(StandardProtocolFamily.UNIX)) {
                System.out.println("     ssc=" + ch.getClass().getName());
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        });
        step("SocketChannel.open(UNIX)", () -> {
            try (SocketChannel ch = SocketChannel.open(StandardProtocolFamily.UNIX)) {
                System.out.println("     sc=" + ch.getClass().getName());
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        });

        // Full round trip, if we get that far.
        File tmp = File.createTempFile("uds-probe-", ".sock");
        String sockPath = tmp.getAbsolutePath();
        if (!tmp.delete()) {
            System.out.println("FAIL could not delete temp file " + sockPath);
            return;
        }
        try {
            SocketAddress sa = java.net.UnixDomainSocketAddress.of(sockPath);
            try (ServerSocketChannel server = ServerSocketChannel.open(StandardProtocolFamily.UNIX)) {
                server.bind(sa, 50);
                System.out.println("OK   bind -> local=" + server.getLocalAddress());
                Thread t = new Thread(() -> {
                    try (SocketChannel s = server.accept()) {
                        ByteBuffer in = ByteBuffer.allocate(64);
                        int n = s.read(in);
                        System.out.println("OK   server read n=" + n + " text="
                                + new String(in.array(), 0, Math.max(n, 0)));
                        s.write(ByteBuffer.wrap("pong".getBytes()));
                    } catch (Throwable e) {
                        System.out.println("FAIL server side -> " + e);
                    }
                });
                t.start();
                try (SocketChannel client = SocketChannel.open(StandardProtocolFamily.UNIX)) {
                    client.connect(sa);
                    System.out.println("OK   client connect");
                    client.write(ByteBuffer.wrap("ping".getBytes()));
                    ByteBuffer resp = ByteBuffer.allocate(64);
                    int n = client.read(resp);
                    System.out.println("OK   client read n=" + n + " text="
                            + new String(resp.array(), 0, Math.max(n, 0)));
                }
                t.join(10000);
            }
        } catch (Throwable e) {
            System.out.println("FAIL round trip -> " + e);
            e.printStackTrace(System.out);
        } finally {
            new File(sockPath).delete();
        }
        System.out.println("DONE");
    }
}
