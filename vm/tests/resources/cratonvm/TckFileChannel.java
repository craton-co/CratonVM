package cratonvm;
import java.io.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;
public class TckFileChannel {
    public static int fc_open_write_read() {
        try {
            Path tmp = Files.createTempFile("tck", ".dat");
            try {
                FileChannel wc = FileChannel.open(tmp, StandardOpenOption.WRITE);
                ByteBuffer wb = ByteBuffer.wrap(new byte[]{1, 2, 3, 4});
                wc.write(wb);
                wc.close();
                FileChannel rc = FileChannel.open(tmp, StandardOpenOption.READ);
                ByteBuffer rb = ByteBuffer.allocate(4);
                rc.read(rb);
                rc.close();
                rb.flip();
                return (rb.get() == 1 && rb.get() == 2 && rb.get() == 3 && rb.get() == 4) ? 1 : 0;
            } finally { Files.deleteIfExists(tmp); }
        } catch (Exception e) { return 0; }
    }
    public static int fc_position() {
        try {
            Path tmp = Files.createTempFile("tck", ".dat");
            try {
                FileChannel fc = FileChannel.open(tmp, StandardOpenOption.WRITE, StandardOpenOption.READ);
                ByteBuffer wb = ByteBuffer.wrap(new byte[]{10, 20, 30});
                fc.write(wb);
                fc.position(1);
                ByteBuffer rb = ByteBuffer.allocate(1);
                fc.read(rb);
                fc.close();
                rb.flip();
                return rb.get() == 20 ? 1 : 0;
            } finally { Files.deleteIfExists(tmp); }
        } catch (Exception e) { return 0; }
    }
    public static int fc_size() {
        try {
            Path tmp = Files.createTempFile("tck", ".dat");
            try {
                FileChannel fc = FileChannel.open(tmp, StandardOpenOption.WRITE);
                fc.write(ByteBuffer.wrap(new byte[10]));
                long sz = fc.size();
                fc.close();
                return sz == 10 ? 1 : 0;
            } finally { Files.deleteIfExists(tmp); }
        } catch (Exception e) { return 0; }
    }
    public static int fc_truncate() {
        try {
            Path tmp = Files.createTempFile("tck", ".dat");
            try {
                FileChannel fc = FileChannel.open(tmp, StandardOpenOption.WRITE);
                fc.write(ByteBuffer.wrap(new byte[100]));
                fc.truncate(50);
                long sz = fc.size();
                fc.close();
                return sz == 50 ? 1 : 0;
            } finally { Files.deleteIfExists(tmp); }
        } catch (Exception e) { return 0; }
    }
}
