import java.lang.foreign.*;
import java.lang.reflect.Field;
import java.nio.ByteOrder;

/** SBR3 lane FFM: the Unsafe atomics over Unsafe.allocateMemory memory and over an Arena segment's address,
 *  and Unsafe.copySwapMemory between native memory and a heap array. One deterministic line per row; diffed
 *  against HotSpot 25 in both modes. */
public class Sbr3FfmUnsafe {
    static sun.misc.Unsafe U;

    interface F { Object get() throws Throwable; }
    static String only = null;
    static void p(String tag, F f) {
        if (only != null && !tag.contains(only)) return;
        String v;
        try { v = String.valueOf(f.get()); }
        catch (Throwable e) { v = "THREW " + e.getClass().getName() + ": " + e.getMessage(); }
        System.out.println(tag + " |" + v + "|");
    }

    public static void main(String[] a) throws Throwable {
        if (a.length > 0) only = a[0];
        Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        U = (sun.misc.Unsafe) f.get(null);

        p("int.cas", () -> { long m = U.allocateMemory(16); U.putInt(m, 5); boolean a1 = U.compareAndSwapInt(null, m, 5, 6); boolean a2 = U.compareAndSwapInt(null, m, 5, 7); int v = U.getInt(m); U.freeMemory(m); return a1 + " " + a2 + " " + v; });
        p("long.cas", () -> { long m = U.allocateMemory(16); U.putLong(m, 5L); boolean a1 = U.compareAndSwapLong(null, m, 5L, 6L); boolean a2 = U.compareAndSwapLong(null, m, 5L, 7L); long v = U.getLong(m); U.freeMemory(m); return a1 + " " + a2 + " " + v; });
        p("int.getAndAdd", () -> { long m = U.allocateMemory(16); U.putInt(m, 5); int o = U.getAndAddInt(null, m, 3); int v = U.getInt(m); U.freeMemory(m); return o + " " + v; });
        p("long.getAndAdd", () -> { long m = U.allocateMemory(16); U.putLong(m, 5L); long o = U.getAndAddLong(null, m, 3L); long v = U.getLong(m); U.freeMemory(m); return o + " " + v; });
        p("int.getAndSet", () -> { long m = U.allocateMemory(16); U.putInt(m, 5); int o = U.getAndSetInt(null, m, 9); int v = U.getInt(m); U.freeMemory(m); return o + " " + v; });
        p("long.getAndSet", () -> { long m = U.allocateMemory(16); U.putLong(m, 5L); long o = U.getAndSetLong(null, m, 9L); long v = U.getLong(m); U.freeMemory(m); return o + " " + v; });
        p("int.cas.offset4", () -> { long m = U.allocateMemory(16); U.putInt(m + 4, 11); boolean a1 = U.compareAndSwapInt(null, m + 4, 11, 12); int v = U.getInt(m + 4); int z = U.getInt(m); U.freeMemory(m); return a1 + " " + v + " " + z; });
        p("int.cas.contended", () -> {
            long m = U.allocateMemory(16); U.putInt(m, 0);
            Thread[] t = new Thread[4];
            for (int i = 0; i < t.length; i++) { t[i] = new Thread(() -> { for (int k = 0; k < 2000; k++) { int o; do { o = U.getInt(m); } while (!U.compareAndSwapInt(null, m, o, o + 1)); } }); t[i].start(); }
            for (Thread x : t) x.join();
            int v = U.getInt(m); U.freeMemory(m); return v;
        });
        p("int.getAndAdd.contended", () -> {
            long m = U.allocateMemory(16); U.putInt(m, 0);
            Thread[] t = new Thread[4];
            for (int i = 0; i < t.length; i++) { t[i] = new Thread(() -> { for (int k = 0; k < 2000; k++) U.getAndAddInt(null, m, 1); }); t[i].start(); }
            for (Thread x : t) x.join();
            int v = U.getInt(m); U.freeMemory(m); return v;
        });
        p("segment.address.cas", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16, 8); s.set(ValueLayout.JAVA_INT, 0, 5); boolean a1 = U.compareAndSwapInt(null, s.address(), 5, 6); return a1 + " " + s.get(ValueLayout.JAVA_INT, 0); } });
        p("segment.address.getAndAdd", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(16, 8); s.set(ValueLayout.JAVA_LONG, 0, 5L); long o = U.getAndAddLong(null, s.address(), 3L); return o + " " + s.get(ValueLayout.JAVA_LONG, 0); } });
        p("copySwap.native.to.array", () -> { long m = U.allocateMemory(16); U.putInt(m, 0x01020304); int[] b = new int[2]; U.copyMemory(null, m, b, sun.misc.Unsafe.ARRAY_INT_BASE_OFFSET, 8); U.freeMemory(m); return Integer.toHexString(b[0]); });
        p("copySwap.segment.to.array", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); s.set(ValueLayout.JAVA_INT, 0, 0x01020304); int[] b = new int[2]; MemorySegment.copy(s, ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 0, b, 0, 2); return Integer.toHexString(b[0]); } });
        p("copySwap.array.to.segment", () -> { try (Arena x = Arena.ofConfined()) { MemorySegment s = x.allocate(8); int[] b = {0x01020304, 0x05060708}; MemorySegment.copy(b, 0, s, ValueLayout.JAVA_INT.withOrder(ByteOrder.BIG_ENDIAN), 0, 2); return Integer.toHexString(s.get(ValueLayout.JAVA_INT, 0)) + " " + Integer.toHexString(s.get(ValueLayout.JAVA_INT, 4)); } });
    }
}
