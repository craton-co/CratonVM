import java.io.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.file.*;

/**
 * Differential allocator probe. Each mode allocates the same number of bytes
 * per iteration by a different route, so comparing their RSS curves says WHICH
 * allocation retains memory.
 *
 *   array  — new byte[size]                      (Java heap only)
 *   read   — new byte[size] + FileChannel.read    (heap + file I/O)
 *   map    — FileChannel.map(READ_ONLY)           (heap + snapshot + mmap)
 *   mapgc  — map, then System.gc() every 10 iters
 *   unmap  — map, then explicit unmap via the JDK cleaner if reachable
 */
public class MapDiag {
    static long rssKb() {
        try (BufferedReader r = new BufferedReader(new FileReader("/proc/self/status"))) {
            for (String l; (l = r.readLine()) != null; ) {
                if (l.startsWith("VmRSS:")) {
                    return Long.parseLong(l.replaceAll("[^0-9]", ""));
                }
            }
        } catch (Exception e) {
            // fall through
        }
        return -1;
    }

    static void report(String mode, int i) {
        Runtime rt = Runtime.getRuntime();
        System.out.println(mode + " i=" + i
            + " rssMB=" + (rssKb() / 1024)
            + " heapUsedMB=" + ((rt.totalMemory() - rt.freeMemory()) >> 20)
            + " heapTotalMB=" + (rt.totalMemory() >> 20)
            + " heapMaxMB=" + (rt.maxMemory() >> 20));
    }

    public static void main(String[] a) throws Exception {
        String mode = a[0];
        int n = Integer.parseInt(a[1]);
        int size = Integer.parseInt(a[2]) * 1024 * 1024;
        Path p = Path.of("mapdiag-" + size + ".bin");
        if (!Files.exists(p) || Files.size(p) != size) {
            Files.write(p, new byte[size]);
        }
        long sum = 0;
        report(mode, -1);
        for (int i = 0; i < n; i++) {
            switch (mode) {
                case "array" -> {
                    byte[] b = new byte[size];
                    b[0] = 1;
                    b[size - 1] = 2;
                    sum += b[0] + b[size - 1];
                }
                case "read" -> {
                    byte[] b = new byte[size];
                    try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ)) {
                        ch.read(ByteBuffer.wrap(b));
                    }
                    sum += b[0] + b[size - 1];
                }
                case "map", "mapgc" -> {
                    try (FileChannel ch = FileChannel.open(p, StandardOpenOption.READ)) {
                        MappedByteBuffer m = ch.map(FileChannel.MapMode.READ_ONLY, 0, ch.size());
                        sum += m.get(0) + m.get(size - 1);
                    }
                    if (mode.equals("mapgc") && i % 10 == 0) {
                        System.gc();
                    }
                }
                default -> throw new IllegalArgumentException(mode);
            }
            if (i % 20 == 0) {
                report(mode, i);
            }
        }
        report(mode, n);
        System.out.println("sum=" + sum);
    }
}
