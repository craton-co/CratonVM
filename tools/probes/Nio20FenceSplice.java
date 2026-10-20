import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.ref.Reference;
import java.nio.ByteBuffer;
import java.util.Arrays;

/**
 * Round nio20 wave 1, lane L1 (NIO19-1, NIO19-3): the optimizing tier's splice of a callee whose
 * only handler is a {@code finally { Reference.reachabilityFence(..); }}, and of
 * {@code Buffer.session()} whose unloaded {@code checkcast} sits on an arm a heap buffer never takes.
 *
 * <p>Bench rows print {@code <row> <N> ns/op}; conformance rows print {@code conf <fact>} lines that
 * must match HotSpot byte for byte (no timings in them). {@code -Dnio20.rows=a,b} selects rows
 * (default all), {@code -Dnio20.n=} the timed ops per row (default 2 000 000).
 *
 * <pre>
 *   javac -d /tmp/p20 tools/probes/Nio20FenceSplice.java
 *   cratonvm -cp /tmp/p20 Nio20FenceSplice                 # arm A (defaults: both switches on)
 *   CRATONVM_JIT_IR_SPLICE_FENCE_FINALLY=0 CRATONVM_JIT_IR_SPLICE_COLD_UNLOADED_TYPECHECK=0 \
 *     cratonvm -cp /tmp/p20 Nio20FenceSplice               # arm B (the refusals restored)
 *   CRATONVM_DBG_JITC=1 ... 2>&1 | grep -c 'callee-exception-table'   # refusal accounting
 * </pre>
 *
 * <p>Round nio20 wave 3, lane L3 (W2L2-3): rows {@code fencetry-osr}, {@code divtry-osr},
 * {@code fencetry} and the {@code conf fencetry-* / divtry-*} facts run loops inside a try/catch.
 * A/B: {@code CRATONVM_JIT_OSR_SPLICE_PRECISE_NOTHROW=0} restores the empty OSR splice plan; with
 * {@code CRATONVM_DBG_JITC=1} the door prints {@code osr-splice-planned ...reachabilityFence} /
 * {@code ...mix} and {@code osr-splice-refused ...div ... precise exception frames}.
 * {@code -Dnio20.rows=fencetry-osr,divtry-osr,fencetry,conf}.
 */
public class Nio20FenceSplice {
    static final String ROWS = System.getProperty("nio20.rows", "all");
    static final int N = Integer.getInteger("nio20.n", 2_000_000);
    static final Object KEEP = new Object();
    static volatile long sink;

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    // The javac `finally` shape the IR tier now splices: value-returning (handler at the tail) ...
    static int fenced(int[] a, int i, Object o) {
        try {
            return a[i] + 1;
        } finally {
            Reference.reachabilityFence(o);
        }
    }

    // ... and void (the handler between a `goto` and the `return`), fencing two references.
    static void fencedVoid(int[] a, int i, Object o, Object p) {
        try {
            a[i] += 1;
        } finally {
            Reference.reachabilityFence(o);
            Reference.reachabilityFence(p);
        }
    }

    static long fenceLoop(int[] a, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += fenced(a, k & 7, KEEP);
            fencedVoid(a, k & 7, KEEP, a);
        }
        return s;
    }

    // Round nio20 wave 3, lane L3 (W2L2-3): loops that sit inside a try/catch. The method has an
    // exception table, so its single-pass OSR body compiles with precise exception frames, which
    // used to clear the whole splice plan: `reachabilityFence` (an empty static body) and `mix` (a
    // static body that cannot throw) were full compiled CALLs every iteration. `div` can throw
    // (`idiv`) and stays a call.
    static int mix(int x) {
        return x * 31 + 7;
    }

    static int div(int x, int d) {
        return x / d;
    }

    static long fenceTryLoop(int n, Object o) {
        long s = 0;
        try {
            for (int k = 0; k < n; k++) {
                Reference.reachabilityFence(o);
                s += mix(k);
            }
        } catch (RuntimeException e) {
            s = -1;
        }
        return s;
    }

    // A throw from the loop body itself (not from a spliced callee) still reaches the handler with
    // the loop's own state.
    static long fenceTryArray(int[] a, int n) {
        long s = 0;
        int k = 0;
        try {
            for (; k < n; k++) {
                Reference.reachabilityFence(a);
                s += mix(a[k]);
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            return -(s * 1000 + k);
        }
        return s;
    }

    static long divTryLoop(int n, int zeroAt) {
        long s = 0;
        try {
            for (int k = 0; k < n; k++) {
                s += div(k, k == zeroAt ? 0 : (k & 7) + 1);
            }
        } catch (ArithmeticException e) {
            return -s;
        }
        return s;
    }

    // One long call, so the loop runs in the single-pass OSR body (the door this row exercises).
    static void benchOnce(String row, Op op) {
        if (!on(row)) {
            return;
        }
        long total = 5L * N;
        long t0 = System.nanoTime();
        sink += op.run((int) total);
        long t1 = System.nanoTime();
        System.out.println(row + " " + ((t1 - t0) / total) + " ns/op");
    }

    static long heapGetInt(ByteBuffer b, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += b.getInt((k & 3) << 2);
        }
        return s;
    }

    static long heapPutInt(ByteBuffer b, int n) {
        for (int k = 0; k < n; k++) {
            b.putInt((k & 3) << 2, k);
        }
        return b.getInt(0);
    }

    static long bulkPut(ByteBuffer dst, ByteBuffer src, int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            dst.clear();
            src.clear();
            dst.put(src);
            s += dst.position();
        }
        return s;
    }

    interface Op {
        long run(int n);
    }

    static void bench(String row, Op op) {
        if (!on(row)) {
            return;
        }
        for (int w = 0; w < 5; w++) {
            sink += op.run(N / 5);
        }
        long t0 = System.nanoTime();
        sink += op.run(N);
        long t1 = System.nanoTime();
        System.out.println(row + " " + ((t1 - t0) / N) + " ns/op");
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + " " + t.getMessage();
    }

    public static void main(String[] args) {
        int[] a = new int[8];
        ByteBuffer heap = ByteBuffer.allocate(16);
        ByteBuffer src = ByteBuffer.allocate(64);
        ByteBuffer dst = ByteBuffer.allocate(64);

        bench("fence", n -> fenceLoop(a, n));
        bench("heapgetint", n -> heapGetInt(heap, n));
        bench("heapputint", n -> heapPutInt(heap, n));
        bench("bulkput", n -> bulkPut(dst, src, n));
        // W2L2-3: the try/catch rows. `*-osr` rows time ONE long call (the OSR body).
        benchOnce("fencetry-osr", n -> fenceTryLoop(n, KEEP));
        benchOnce("divtry-osr", n -> divTryLoop(n, -1));
        bench("fencetry", n -> fenceTryLoop(n, KEEP));

        if (on("conf")) {
            // Warm the shapes first, so the facts below run against compiled callers.
            for (int w = 0; w < 20; w++) {
                sink += fenceLoop(a, 100_000);
                sink += heapGetInt(heap, 100_000);
            }
            // An exception raised inside a spliced fence body propagates as itself.
            try {
                sink += fenced(a, 99, KEEP);
                System.out.println("conf fenced-aioobe NO-THROW");
            } catch (ArrayIndexOutOfBoundsException e) {
                System.out.println("conf fenced-aioobe " + ex(e));
            }
            try {
                fencedVoid(null, 0, KEEP, a);
                System.out.println("conf fencedvoid-npe NO-THROW");
            } catch (NullPointerException e) {
                System.out.println("conf fencedvoid-npe " + e.getClass().getName());
            }
            int[] fresh = new int[8];
            System.out.println("conf fence-sum " + fenceLoop(fresh, 1000) + " " + Arrays.toString(fresh));
            // W2L2-3: the try/catch loops, first run long enough to OSR, then the facts.
            sink += fenceTryLoop(3_000_000, KEEP);
            sink += divTryLoop(3_000_000, -1);
            int[] small = new int[64];
            for (int i = 0; i < small.length; i++) {
                small[i] = i * 3;
            }
            sink += fenceTryArray(small, 64);
            System.out.println("conf fencetry-sum " + fenceTryLoop(100_000, KEEP));
            System.out.println("conf fencetry-aioobe " + fenceTryArray(small, 100));
            System.out.println("conf divtry-zero " + divTryLoop(100_000, 77_777));
            System.out.println("conf divtry-sum " + divTryLoop(100_000, -1));
            // The compiled heap-buffer caller meets a segment-backed buffer: `session()` takes the
            // cast arm its profile never ran (the precise exit into the interpreter).
            ByteBuffer seg = MemorySegment.ofArray(new byte[16]).asByteBuffer();
            seg.putInt(0, 42).putInt(4, 7).putInt(8, -1).putInt(12, 1 << 20);
            System.out.println("conf segment-getint " + heapGetInt(seg, 4) + " " + heapGetInt(seg, 1000));
            heap.putInt(0, 3).putInt(4, 5).putInt(8, 11).putInt(12, 13);
            System.out.println("conf heap-getint " + heapGetInt(heap, 4));
            // A closed confined arena: the session check's conversion is unchanged.
            Arena arena = Arena.ofConfined();
            ByteBuffer closed = arena.allocate(16).asByteBuffer();
            arena.close();
            try {
                sink += closed.getInt(0);
                System.out.println("conf closed NO-THROW");
            } catch (IllegalStateException e) {
                System.out.println("conf closed " + ex(e));
            }
        }
        System.out.println("done");
    }
}
