import java.io.RandomAccessFile;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Second cut at the zero-copy failure. The first probe used
 * FileChannel.open(Path) and a heap-backed WritableByteChannel, and both VMs
 * transferred 951/951 -- so the defect is in one of the two variables that
 * probe did NOT reproduce from Netty's DefaultFileRegion:
 *
 *   1. the FileChannel comes from `new RandomAccessFile(f, "r").getChannel()`,
 *      not from FileChannel.open(Path);
 *   2. the target is a real SocketChannel, not an in-heap channel.
 *
 * Four combinations, so whichever variable matters is named rather than
 * inferred.
 */
public class TransferToSocketProbe {

    static Path file;
    static byte[] payload;

    static FileChannel viaOpen() throws Exception {
        return FileChannel.open(file, StandardOpenOption.READ);
    }

    static FileChannel viaRaf() throws Exception {
        return new RandomAccessFile(file.toFile(), "r").getChannel();
    }

    /** Drains a socket pair and reports how many bytes actually arrived. */
    static long toSocket(FileChannel fc, String label) throws Exception {
        try (ServerSocketChannel ssc = ServerSocketChannel.open()) {
            ssc.bind(new InetSocketAddress("127.0.0.1", 0));
            final int port = ((InetSocketAddress) ssc.getLocalAddress()).getPort();

            final long[] received = { -1 };
            Thread reader = new Thread(() -> {
                try (SocketChannel s = ssc.accept()) {
                    ByteBuffer bb = ByteBuffer.allocate(8192);
                    long total = 0;
                    int n;
                    while ((n = s.read(bb)) > 0) { total += n; bb.clear(); }
                    received[0] = total;
                } catch (Exception e) {
                    received[0] = -2;
                }
            });
            reader.start();

            long returned;
            try (SocketChannel c = SocketChannel.open(new InetSocketAddress("127.0.0.1", port))) {
                returned = fc.transferTo(0, Files.size(file), c);
            }
            reader.join(15000);
            System.out.printf("  %-28s returned=%-6d socketReceived=%d%n",
                    label, returned, received[0]);
            return returned;
        } finally {
            fc.close();
        }
    }

    public static void main(String[] args) throws Exception {
        file = Files.createTempFile("xfersock", ".bin");
        payload = new byte[951];
        for (int i = 0; i < payload.length; i++) payload[i] = (byte) (i % 251);
        Files.write(file, payload);
        System.out.println("file size = " + Files.size(file));

        System.out.println("== FileChannel source, SocketChannel target ==");
        toSocket(viaOpen(), "FileChannel.open");
        toSocket(viaRaf(), "RandomAccessFile.getChannel");

        // Is the RAF-derived channel sane at all, independent of transferTo?
        System.out.println("== RAF channel sanity ==");
        try (FileChannel fc = viaRaf()) {
            System.out.println("  size()=" + fc.size() + "  position()=" + fc.position());
            ByteBuffer bb = ByteBuffer.allocate(2048);
            int n = fc.read(bb);
            System.out.println("  read()=" + n + " (expected 951)");
        }
        Files.deleteIfExists(file);
    }
}
