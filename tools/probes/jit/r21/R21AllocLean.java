// JIT round 21 wave 1, lane alloc: lean compiled allocation, decomposed.
//
// Target (r20 handoff perf table, R20PAllocTlabTls): lean `new byte[]` / `new long[]` /
// `new Object` cost 1002 / 1080 / 751 ns (529 / 636 / ~751 after CRATONVM_JIT_IR_INLINE_PUTSTATIC_REF)
// against HotSpot 80 / 50 / 203. The compiled inline sequence itself is ~75 instructions on ZGC
// (docs/known-issues/gc/zgc-zr7ca-...: 54 ns per allocation, HotSpot 9.76), so hundreds of ns per
// allocation mean a Rust call per allocation, a refill per allocation, or a body that is not the
// optimizing tier's. This probe separates those: the same allocation under five store shapes, the
// size ladder 0 / 1 / 17 / large, Object / Object[] / an object with fields, and a store-only
// baseline row.
//
// Run lines (build: javac -d tools/probes/out tools/probes/jit/r21/R21AllocLean.java):
//   P=tools/probes/out
//   cratonvm -Xmx512m -cp $P R21AllocLean                           # ZGC (the default collector)
//   cratonvm -XX:+UseG1GC -Xmx512m -cp $P R21AllocLean
//   cratonvm -XX:+UseGenerationalGC -Xmx512m -cp $P R21AllocLean
//   cratonvm --compatible -Xmx512m -cp $P R21AllocLean              # both modes, same lines
//   java -XX:+UseZGC -Xmx512m -cp $P R21AllocLean                   # HotSpot 25, and with
//   java -XX:+UseG1GC -Xmx512m -cp $P R21AllocLean                  #   -XX:+UseSerialGC for the
//                                                                   #   Generational comparison
// Engagement census (one run per collector, NOT for timing): prefix
//   CRATONVM_DBG_JIT_METHOD_STATS=1
// and read the exit line `[cratonvm] compiled-new helper entries: jit_new_object_calls=
// jit_post_tlab_init_calls= jit_anewarray_calls= ... jit_newarray_calls= jit_newarray_small_calls=
// jit_newarray_tlab_served= jit_zgc_note_calls=` (r21 alloc keys). With the inline bump engaged,
// jit_newarray_small_calls / jit_new_object_calls are ~ one per TLAB refill (thousands), not
// ~ the row's allocation count (each timed row does 9 x 2,000,000 allocations plus warm-up);
// jit_zgc_note_calls near the allocation count = the ZGC inline start-bit store refuses (table
// blocked during marking, epoch moved) and every allocation pays the Rust announce;
// jit_post_tlab_init_calls near the count = the `new` class is not a post-init no-op.
// Per-site: CRATONVM_DBG_JITC=1 prints `ir inline-TLAB newarray EMITTED` / `ir inline-TLAB bump
// DECLINED: <why>` / `ir lean body: ... thread=tls N thread=frame M` per compiled site.
//
// HotSpot expectation: every non-`time` line identical (diff them); time rows ~5-20 ns for small
// allocations under C2 (the ring rows a little more, the large row is the zeroing of 64 KiB).
// Pass for this lane's target: lean-static-* rows within ~3x HotSpot's on each collector.

public class R21AllocLean {
    static Object sink;
    static byte[] sinkB;
    static long[] sinkL;
    static Object[] ring = new Object[1024];
    static int acc;

    static final int N = 2_000_000;
    static final int REPS = 9;

    static final class P {
        int a;
        long b;
        double c;
        Object d;
        boolean e;
        float f;
        char g;
        short h;
        byte i;
    }

    static final class Q {
        Object x;
        Object y;
    }

    // ---- kernels: one allocation per iteration, result escapes ----------------------------

    static void baseline(int n) {
        Object k = ring;
        for (int i = 0; i < n; i++) {
            sink = k;
        }
    }

    static void leanByte16Static(int n) {
        for (int i = 0; i < n; i++) {
            sinkB = new byte[16];
        }
    }

    static void leanLong16Static(int n) {
        for (int i = 0; i < n; i++) {
            sinkL = new long[16];
        }
    }

    static void leanObjectStatic(int n) {
        for (int i = 0; i < n; i++) {
            sink = new Object();
        }
    }

    static void leanObjArr16Static(int n) {
        for (int i = 0; i < n; i++) {
            sink = new Object[16];
        }
    }

    static void leanFieldsStatic(int n) {
        for (int i = 0; i < n; i++) {
            sink = new P();
        }
    }

    static void leanRefFieldsStatic(int n) {
        for (int i = 0; i < n; i++) {
            sink = new Q();
        }
    }

    static void ringByte16(int n) {
        Object[] r = ring;
        for (int i = 0; i < n; i++) {
            r[i & 1023] = new byte[16];
        }
    }

    static void ringObject(int n) {
        Object[] r = ring;
        for (int i = 0; i < n; i++) {
            r[i & 1023] = new Object();
        }
    }

    // A primitive sink: the array escapes only into its length read; HotSpot may scalar-replace
    // it (EA), which is why this row is the "no escape" control, not a target.
    static void localByte16(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[16];
            s += b.length;
        }
        acc += s;
    }

    static void sizedByteStatic(int n, int len) {
        for (int i = 0; i < n; i++) {
            sinkB = new byte[len];
        }
    }

    // A variable length (the R20 shape keeps the length constant; this one cannot be folded).
    static void varLongStatic(int n, int mask) {
        for (int i = 0; i < n; i++) {
            sinkL = new long[i & mask];
        }
    }

    // ---- the lean-body shape: an allocating method with nothing live across its GC point ---

    static byte[] mkByte(int n) {
        return new byte[n];
    }

    static long[] mkLong(int n) {
        return new long[n];
    }

    static Object[] mkObjArr(int n) {
        return new Object[n];
    }

    static Object mkObject() {
        return new Object();
    }

    static void callByte16(int n) {
        for (int i = 0; i < n; i++) {
            sinkB = mkByte(16);
        }
    }

    // ---- timing --------------------------------------------------------------------------

    interface Kernel {
        void run(int n);
    }

    static void time(String row, Kernel k) {
        time(row, k, 1);
    }

    // `R21AllocLean <row>` runs that one timed row and nothing else (no correctness lines), so a
    // census run (`CRATONVM_DBG_JIT_METHOD_STATS=1`) reads one row's helper entries and refills.
    static String only;

    static long gcCount() {
        long c = 0;
        for (java.lang.management.GarbageCollectorMXBean b
                : java.lang.management.ManagementFactory.getGarbageCollectorMXBeans()) {
            c += Math.max(0, b.getCollectionCount());
        }
        return c;
    }

    static long gcMillis() {
        long t = 0;
        for (java.lang.management.GarbageCollectorMXBean b
                : java.lang.management.ManagementFactory.getGarbageCollectorMXBeans()) {
            t += Math.max(0, b.getCollectionTime());
        }
        return t;
    }

    static long cpuNanos() {
        java.lang.management.ThreadMXBean t = java.lang.management.ManagementFactory.getThreadMXBean();
        return t.isCurrentThreadCpuTimeSupported() ? t.getCurrentThreadCpuTime() : -1;
    }

    // `perOp`: how many kernel units one allocation is (the 64 KiB row runs n / 64 allocations).
    // Besides the median wall time, each row prints (all over the 9 timed reps):
    //   time <row>-cpu    this thread's CPU per allocation (a collection run on this thread counts)
    //   time <row>-gcms   collector-reported collection time per allocation, in ns
    //   time <row>-gcs    collections per million allocations (printed in the ns/op slot)
    // so a row's cost splits into mutator, collection, and waiting.
    static void time(String row, Kernel k, int perOp) {
        if (only != null && !only.equals(row)) {
            return;
        }
        for (int w = 0; w < 5; w++) {
            k.run(N / 4);
        }
        double[] ns = new double[REPS];
        long c0 = gcCount(), m0 = gcMillis(), cpu0 = cpuNanos();
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            k.run(N);
            long t1 = System.nanoTime();
            ns[r] = (t1 - t0) / (double) (N / perOp);
        }
        long c1 = gcCount(), m1 = gcMillis(), cpu1 = cpuNanos();
        double allocs = (double) REPS * (N / perOp);
        java.util.Arrays.sort(ns);
        System.out.printf(java.util.Locale.ROOT, "time %s %.1f ns/op%n", row, ns[REPS / 2]);
        if (cpu0 >= 0) {
            System.out.printf(java.util.Locale.ROOT, "time %s-cpu %.1f ns/op%n", row,
                    (cpu1 - cpu0) / allocs);
        }
        System.out.printf(java.util.Locale.ROOT, "time %s-gcms %.1f ns/op%n", row,
                (m1 - m0) * 1e6 / allocs);
        System.out.printf(java.util.Locale.ROOT, "time %s-gcs %.2f ns/op%n", row,
                (c1 - c0) * 1e6 / allocs);
    }

    // ---- correctness ---------------------------------------------------------------------

    static int nonZeroBytes(byte[] b) {
        int c = 0;
        for (byte x : b) {
            if (x != 0) c++;
        }
        return c;
    }

    static int nonZeroLongs(long[] b) {
        int c = 0;
        for (long x : b) {
            if (x != 0) c++;
        }
        return c;
    }

    static int nonNull(Object[] b) {
        int c = 0;
        for (Object x : b) {
            if (x != null) c++;
        }
        return c;
    }

    // Dirty the TLAB with filled arrays of the same shape, drop them, then allocate fresh ones
    // from compiled code and count any element that is not the default.
    static void zeroedContents() {
        int[] lens = {0, 1, 7, 8, 17, 255, 4096, 40000};
        int badB = 0, badL = 0, badO = 0, badLen = 0;
        for (int round = 0; round < 3000; round++) {
            for (int len : lens) {
                byte[] d = new byte[len];
                java.util.Arrays.fill(d, (byte) 0x5A);
                long[] dl = new long[len];
                java.util.Arrays.fill(dl, -1L);
                Object[] dO = new Object[len];
                java.util.Arrays.fill(dO, d);
                sink = d;
                sink = dl;
                sink = dO;
                byte[] b = mkByte(len);
                long[] l = mkLong(len);
                Object[] o = mkObjArr(len);
                badB += nonZeroBytes(b);
                badL += nonZeroLongs(l);
                badO += nonNull(o);
                if (b.length != len || l.length != len || o.length != len) badLen++;
            }
        }
        System.out.println("zeroed byte[] nonzero=" + badB + " long[] nonzero=" + badL
                + " Object[] nonnull=" + badO + " length-mismatch=" + badLen);
    }

    static void fieldDefaults() {
        int bad = 0;
        for (int i = 0; i < 200_000; i++) {
            P p = new P();
            if (p.a != 0 || p.b != 0L || p.c != 0.0 || p.d != null || p.e || p.f != 0.0f
                    || p.g != 0 || p.h != 0 || p.i != 0) {
                bad++;
            }
            p.a = i;
            p.b = -1L;
            p.c = 1.5;
            p.d = p;
            p.e = true;
            p.f = 2.5f;
            p.g = 'x';
            p.h = 7;
            p.i = 9;
            sink = p;
        }
        System.out.println("field defaults bad=" + bad);
    }

    static void sizeLadder() {
        int[] lens = {0, 1, 17, 4095, 4096, 4097, 32768, 100_000, 1 << 20};
        StringBuilder sb = new StringBuilder("size ladder");
        for (int len : lens) {
            byte[] b = mkByte(len);
            long[] l = mkLong(len);
            Object[] o = mkObjArr(len);
            sb.append(' ').append(len).append(':').append(b.length).append('/').append(l.length)
                    .append('/').append(o.length).append('/')
                    .append(b.getClass().getName()).append(l.getClass().getName())
                    .append(o.getClass().getName());
        }
        System.out.println(sb);
    }

    static void hashStability() {
        Object[] keep = new Object[64];
        int[] h = new int[64];
        for (int i = 0; i < keep.length; i++) {
            keep[i] = (i % 3 == 0) ? mkObject() : (i % 3 == 1) ? mkByte(i) : mkLong(i);
            h[i] = System.identityHashCode(keep[i]);
        }
        // Churn enough to collect (and, on a moving collector, relocate) several times.
        for (int i = 0; i < 4_000_000; i++) {
            sink = new long[8];
        }
        System.gc();
        int moved = 0;
        for (int i = 0; i < keep.length; i++) {
            if (System.identityHashCode(keep[i]) != h[i]) moved++;
        }
        boolean distinct = keep[0] != keep[3] && keep[1] != keep[4];
        System.out.println("identityHashCode unstable=" + moved + " distinct=" + distinct);
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static void exceptionTexts() {
        // Warm the allocating methods so the throws below come from compiled code.
        for (int i = 0; i < 50_000; i++) {
            sinkB = mkByte(i & 31);
            sinkL = mkLong(i & 31);
            sink = mkObjArr(i & 31);
        }
        int[] bad = {-1, -2147483648};
        for (int n : bad) {
            try {
                mkByte(n);
                System.out.println("byte[" + n + "] no exception");
            } catch (Throwable t) {
                System.out.println("byte[" + n + "] " + describe(t));
            }
            try {
                mkLong(n);
                System.out.println("long[" + n + "] no exception");
            } catch (Throwable t) {
                System.out.println("long[" + n + "] " + describe(t));
            }
            try {
                mkObjArr(n);
                System.out.println("Object[" + n + "] no exception");
            } catch (Throwable t) {
                System.out.println("Object[" + n + "] " + describe(t));
            }
        }
        int[] huge = {Integer.MAX_VALUE, Integer.MAX_VALUE - 1};
        for (int n : huge) {
            try {
                mkByte(n);
                System.out.println("byte[MAX-" + (Integer.MAX_VALUE - n) + "] no exception");
            } catch (Throwable t) {
                System.out.println("byte[MAX-" + (Integer.MAX_VALUE - n) + "] " + describe(t));
            }
            try {
                mkLong(n);
                System.out.println("long[MAX-" + (Integer.MAX_VALUE - n) + "] no exception");
            } catch (Throwable t) {
                System.out.println("long[MAX-" + (Integer.MAX_VALUE - n) + "] " + describe(t));
            }
            try {
                mkObjArr(n);
                System.out.println("Object[MAX-" + (Integer.MAX_VALUE - n) + "] no exception");
            } catch (Throwable t) {
                System.out.println("Object[MAX-" + (Integer.MAX_VALUE - n) + "] " + describe(t));
            }
        }
        // MAX-2 is the largest legal length: on a 512 MiB heap it is a heap-space failure.
        try {
            mkLong(Integer.MAX_VALUE - 2);
            System.out.println("long[MAX-2] no exception");
        } catch (Throwable t) {
            System.out.println("long[MAX-2] " + describe(t));
        }
        // And the VM still allocates after all of that.
        System.out.println("after exceptions " + mkByte(17).length + " " + mkLong(17).length);
    }

    public static void main(String[] args) {
        only = args.length > 0 ? args[0] : null;
        if (only == null) {
            zeroedContents();
            fieldDefaults();
            sizeLadder();
            hashStability();
            exceptionTexts();
        }

        time("baseline-static-store", R21AllocLean::baseline);
        time("lean-static-byte16", R21AllocLean::leanByte16Static);
        time("lean-static-long16", R21AllocLean::leanLong16Static);
        time("lean-static-object", R21AllocLean::leanObjectStatic);
        time("lean-static-objarr16", R21AllocLean::leanObjArr16Static);
        time("lean-static-fields", R21AllocLean::leanFieldsStatic);
        time("lean-static-ref-fields", R21AllocLean::leanRefFieldsStatic);
        time("ring-byte16", R21AllocLean::ringByte16);
        time("ring-object", R21AllocLean::ringObject);
        time("local-byte16", R21AllocLean::localByte16);
        time("call-byte16", R21AllocLean::callByte16);
        time("lean-static-byte0", n -> sizedByteStatic(n, 0));
        time("lean-static-byte1", n -> sizedByteStatic(n, 1));
        time("lean-static-byte17", n -> sizedByteStatic(n, 17));
        time("lean-static-long-var", n -> varLongStatic(n, 31));
        time("lean-static-byte64k", n -> sizedByteStatic(n / 64, 65536), 64);
        // Wave 2: the size ladder between the parity rows (16-40 B) and long16 (144 B), to find
        // where the per-allocation cost turns (total footprint = 16 + body rounded to 8).
        time("lean-static-byte48", n -> sizedByteStatic(n, 48));
        time("lean-static-byte64", n -> sizedByteStatic(n, 64));
        time("lean-static-byte80", n -> sizedByteStatic(n, 80));
        time("lean-static-byte96", n -> sizedByteStatic(n, 96));
        time("lean-static-byte112", n -> sizedByteStatic(n, 112));
        time("lean-static-byte128", n -> sizedByteStatic(n, 128));
        time("lean-static-byte256", n -> sizedByteStatic(n, 256));
        time("lean-static-byte1024", n -> sizedByteStatic(n / 4, 1024), 4);
        System.out.println("done acc=" + (acc != 0));
    }
}
