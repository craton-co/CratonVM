import java.io.IOException;
import java.io.InputStream;
import java.lang.management.*;

/**
 * ONE arm per process, so `CRATONVM_DBG=jit-method-stats` describes that arm
 * and nothing else. `-Darm=boxed|prim|staticcall`.
 *
 * The three arms are identical in shape and differ only in the thing under
 * suspicion, so the method-stats lines can be diffed against each other.
 */
public class TierOneArm {
    static final ThreadMXBean TB = ManagementFactory.getThreadMXBean();
    static final long Q = 15_625_000L;
    static long sink;

    static final class BoxedStream extends InputStream {
        private Long count;
        BoxedStream(long n) { this.count = n; }
        @Override public int read() { if (count > 0) { count--; return 7; } return -1; }
    }
    static final class PrimStream extends InputStream {
        private long count;
        PrimStream(long n) { this.count = n; }
        @Override public int read() { if (count > 0) { count--; return 7; } return -1; }
    }
    static long addStatic(long a, long b) { long s = 0; for (int i = 0; i < 1; i++) s = a + b; return s; }
    static final java.util.concurrent.atomic.AtomicLong AL =
        new java.util.concurrent.atomic.AtomicLong(1);
    /** One get + one CAS, NO do/while — separates the loop from the atomics. */
    static int lcgNoLoop(java.util.concurrent.atomic.AtomicLong seed, int bits) {
        long oldseed = seed.get();
        long nextseed = (oldseed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
        seed.compareAndSet(oldseed, nextseed);
        return (int) (nextseed >>> (48 - bits));
    }
    /** The SAME body as `lcgNoLoop`, but reached by a VIRTUAL call. If this is
     *  fast and the static twin is not, the static-vs-virtual dispatch path is
     *  the difference — `jit_invoke_dispatch`'s compiled-callee-entry cache is
     *  gated `&& !statically_bound`. */
    static class Holder {
        int lcgNoLoopVirtual(java.util.concurrent.atomic.AtomicLong seed, int bits) {
            long oldseed = seed.get();
            long nextseed = (oldseed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
            seed.compareAndSet(oldseed, nextseed);
            return (int) (nextseed >>> (48 - bits));
        }
    }
    static final Holder H = new Holder();
    /** A callee with a REAL loop and no atomics — separates the loop alone. */
    static long sumLoop(long a, int iters) { long s = 0; for (int i = 0; i < iters; i++) s += a + i; return s; }
    /** Exactly java.util.Random.next(int)'s body. */
    static int lcgNext(java.util.concurrent.atomic.AtomicLong seed, int bits) {
        long oldseed, nextseed;
        do {
            oldseed = seed.get();
            nextseed = (oldseed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
        } while (!seed.compareAndSet(oldseed, nextseed));
        return (int) (nextseed >>> (48 - bits));
    }

    public static void main(String[] a) throws IOException {
        String arm = System.getProperty("arm", "boxed");
        int n = Integer.getInteger("n", 3_000_000);
        long c0 = TB.getCurrentThreadCpuTime();
        switch (arm) {
            case "boxed": { BoxedStream in = new BoxedStream(n);
                for (int i = 0; i < n; i++) sink += in.read(); break; }
            case "prim": { PrimStream in = new PrimStream(n);
                for (int i = 0; i < n; i++) sink += in.read(); break; }
            case "staticcall": { for (int i = 0; i < n; i++) sink += addStatic(i, 1); break; }
            case "lcgcall": { for (int i = 0; i < n; i++) sink += lcgNext(AL, 32); break; }
            case "lcgnoloop": { for (int i = 0; i < n; i++) sink += lcgNoLoop(AL, 32); break; }
            case "lcgvirtual": { for (int i = 0; i < n; i++) sink += H.lcgNoLoopVirtual(AL, 32); break; }
            case "sumloop3": { for (int i = 0; i < n; i++) sink += sumLoop(i, 3); break; }
            case "atomiccall": { for (int i = 0; i < n; i++) sink += AL.get(); break; }
            case "lcginline": { for (int i = 0; i < n; i++) {
                long o, ns2; do { o = AL.get(); ns2 = (o * 0x5DEECE66DL + 0xBL) & ((1L<<48)-1); }
                while (!AL.compareAndSet(o, ns2)); sink += (int)(ns2 >>> 16); } break; }
            default: throw new IllegalArgumentException(arm);
        }
        long d = TB.getCurrentThreadCpuTime() - c0;
        System.out.printf("CK arm=%s n=%d %.2f ns/op cpu (ticks=%d) sink=%d%n",
            arm, n, (double) d / n, d / Q, sink == 0 ? 0 : 1);
    }
}
