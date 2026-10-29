import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;

/**
 * Regression: {@code FileChannel.transferTo(.., SocketChannel)} delivers the file.
 *
 * Under {@code --jdk-only} the JDK's own {@code canTransferToDirectly} answers {@code true}, so the
 * transfer reaches {@code FileDispatcherImpl.transferTo0} with the socket's NIO id as destination. That
 * id is not in the file descriptor table; the native answered {@code IOException: Bad file descriptor}
 * (Tomcat {@code TestSendFile}: the client read a short body, {@code Premature EOF}).
 */
public class RTransferToSocket {
    public static void main(String[] args) throws Exception {
        for (boolean nonBlocking : new boolean[] {false, true}) {
            for (int size : new int[] {1, 100_000, 3_000_000}) {
                System.out.println("CK nonBlocking=" + nonBlocking + " size=" + size + " " + run(nonBlocking, size));
            }
        }
        System.out.println("PASS RTransferToSocket (6 checks)");
    }

    private static String run(boolean nonBlocking, int size) throws Exception {
        Path f = Files.createTempFile("rtts", ".dat");
        try {
            byte[] data = new byte[size];
            Arrays.fill(data, (byte) 'X');
            Files.write(f, data);
            long[] got = new long[1];
            try (ServerSocketChannel ss = ServerSocketChannel.open()) {
                ss.bind(new InetSocketAddress("127.0.0.1", 0));
                int port = ((InetSocketAddress) ss.getLocalAddress()).getPort();
                Thread reader = new Thread(() -> {
                    try (SocketChannel c = ss.accept()) {
                        ByteBuffer b = ByteBuffer.allocate(65536);
                        long total = 0;
                        while (c.read(b) >= 0) {
                            total += b.position();
                            b.clear();
                        }
                        got[0] = total;
                    } catch (Exception e) {
                        got[0] = -1;
                    }
                });
                reader.start();
                try (SocketChannel s = SocketChannel.open(new InetSocketAddress("127.0.0.1", port));
                        FileChannel fc = FileChannel.open(f)) {
                    if (nonBlocking) {
                        s.configureBlocking(false);
                    }
                    long pos = 0;
                    while (pos < size) {
                        long n = fc.transferTo(pos, size - pos, s);
                        if (n == 0) {
                            Thread.sleep(1);
                        }
                        pos += n;
                    }
                }
                reader.join();
            }
            return "delivered=" + got[0];
        } finally {
            Files.delete(f);
        }
    }
}
