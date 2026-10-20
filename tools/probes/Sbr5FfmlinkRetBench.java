import java.lang.foreign.*;
import java.lang.management.*;
import java.nio.*;
import java.util.*;

/** SBR5 lane ffmlink: cost of the retired rows (CPU ns per op of THIS thread, median of 7 rounds).
 *  arena.cycle = Arena.ofConfined + allocate(64) + close (Buffer$2.reserveMemory/unreserveMemory);
 *  bb.heap/direct = ByteBuffer.getChar/putChar pair (ScopedMemoryAccess.get/putCharUnaligned);
 *  lookup.lib = libraryLookup + find + close (SymbolLookup rows). */
public class Sbr5FfmlinkRetBench {
    static final ThreadMXBean T = ManagementFactory.getThreadMXBean();
    interface Op { long run(int n); }
    static void bench(String name, int n, Op op) {
        long w0 = System.nanoTime(); // warm for 3 s so the background compiles of the loop and its callees land
        while (System.nanoTime() - w0 < 3_000_000_000L) op.run(Math.max(50, n / 20));
        long[] r = new long[7];
        long sink = 0;
        for (int i = 0; i < r.length; i++) {
            long t0 = T.getCurrentThreadCpuTime();
            sink += op.run(n);
            r[i] = (T.getCurrentThreadCpuTime() - t0) / n;
        }
        Arrays.sort(r);
        System.out.println(name + " ns/op median=" + r[3] + " all=" + Arrays.toString(r) + (sink == 42 ? "!" : ""));
    }
    public static void main(String[] a) throws Throwable {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 20000;
        bench("arena.cycle", n, k -> { long s = 0; for (int i = 0; i < k; i++) try (Arena z = Arena.ofConfined()) { s += z.allocate(64).byteSize(); } return s; });
        ByteBuffer h = ByteBuffer.allocate(64), d = ByteBuffer.allocateDirect(64);
        bench("bb.heap.char", n * 5, k -> { long s = 0; for (int i = 0; i < k; i++) { h.putChar(i & 31, (char) i); s += h.getChar(i & 31); } return s; });
        bench("bb.direct.char", n * 5, k -> { long s = 0; for (int i = 0; i < k; i++) { d.putChar(i & 31, (char) i); s += d.getChar(i & 31); } return s; });
        bench("bb.direct.short", n * 5, k -> { long s = 0; for (int i = 0; i < k; i++) { d.putShort(i & 31, (short) i); s += d.getShort(i & 31); } return s; });
        bench("bb.direct.int", n * 5, k -> { long s = 0; for (int i = 0; i < k; i++) { d.putInt(i & 28, i); s += d.getInt(i & 28); } return s; });
        bench("bb.direct.long", n * 5, k -> { long s = 0; for (int i = 0; i < k; i++) { d.putLong(i & 24, i); s += d.getLong(i & 24); } return s; });
        bench("lookup.lib", n / 20, k -> { long s = 0; for (int i = 0; i < k; i++) try (Arena z = Arena.ofConfined()) { s += SymbolLookup.libraryLookup("libc.so.6", z).find("strlen").get().address() != 0 ? 1 : 0; } return s; });
        bench("lookup.loader", n, k -> { long s = 0; SymbolLookup l = SymbolLookup.loaderLookup(); for (int i = 0; i < k; i++) s += l.find("no_such_symbol_zz").isPresent() ? 1 : 0; return s; });
    }
}
