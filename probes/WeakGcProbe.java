import java.io.File;
import java.io.RandomAccessFile;
import java.lang.ref.WeakReference;
import java.nio.ByteBuffer;
import java.nio.MappedByteBuffer;
import java.nio.channels.FileChannel;

/**
 * Can a weakly-referenced object be collected by repeated `System.gc()`?
 *
 * This is the exact idiom `org.h2.store.fs.niomapped.FileNioMapped.unMap()`
 * uses to force a `MappedByteBuffer` to be released before the file is
 * truncated or deleted (a workaround for JDK-4724038), and it fails there with
 * `IOException: Timeout (10000 ms) reached while trying to GC mapped buffer`.
 * Three referent shapes are checked separately so "weak references do not
 * clear" and "this particular object is retained by something" are
 * distinguishable rather than conflated.
 *
 *   java WeakGcProbe [timeoutMs]
 */
public class WeakGcProbe {

    public static void main(String[] args) throws Exception {
        long timeoutMs = args.length > 0 ? Long.parseLong(args[0]) : 10000;
        int failures = 0;

        failures += check("plain Object", timeoutMs, plainObject());
        failures += check("direct ByteBuffer", timeoutMs, directBuffer());

        File f = File.createTempFile("weakgcprobe", ".tmp");
        f.deleteOnExit();
        try (RandomAccessFile raf = new RandomAccessFile(f, "rw")) {
            raf.setLength(1 << 16);
            try (FileChannel ch = raf.getChannel()) {
                failures += check("MappedByteBuffer", timeoutMs, mappedBuffer(ch));
            }
        }
        f.delete();

        if (failures != 0) {
            System.out.println("FAIL WeakGcProbe: " + failures + " referent(s) never collected");
            System.exit(1);
        }
        System.out.println("OK WeakGcProbe");
    }

    /** Each factory drops its own strong reference before returning. */
    private static WeakReference<Object> plainObject() {
        Object o = new Object();
        WeakReference<Object> r = new WeakReference<>(o);
        return r;
    }

    private static WeakReference<Object> directBuffer() {
        ByteBuffer b = ByteBuffer.allocateDirect(1 << 16);
        b.put(0, (byte) 1);
        return new WeakReference<>((Object) b);
    }

    private static WeakReference<Object> mappedBuffer(FileChannel ch) throws Exception {
        MappedByteBuffer m = ch.map(FileChannel.MapMode.READ_WRITE, 0, 1 << 16);
        m.put(0, (byte) 1);
        m.force();
        return new WeakReference<>((Object) m);
    }

    private static int check(String what, long timeoutMs, WeakReference<Object> ref) {
        long start = System.nanoTime();
        int cycles = 0;
        while (ref.get() != null) {
            if ((System.nanoTime() - start) / 1_000_000L > timeoutMs) {
                System.out.printf("  %-20s NOT COLLECTED after %d System.gc() calls / %d ms%n",
                        what, cycles, timeoutMs);
                return 1;
            }
            System.gc();
            cycles++;
            Thread.yield();
        }
        System.out.printf("  %-20s collected after %d System.gc() call(s), %.1f ms%n",
                what, cycles, (System.nanoTime() - start) / 1e6);
        return 0;
    }
}
