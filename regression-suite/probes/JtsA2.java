// JIT round 20 wave 11, lane pNat, item L5W17-5 (book jit-r21-nio-proposals-w17-L5.md;
// switch CRATONVM_FAST_THROW_UNLOCKED_HANDOUT, default ON).
//
// A hot caught implicit exception at a site the fast throw (OmitStackTraceInFastThrow) has
// made hot hands out this VM's ONE preallocated instance per kind. ON, the hand-out reads the
// instance's JNI global-ref slot without the table's mutex (and detailMessage's slot from the
// per-VM cell) instead of `jni_global_refs.lock().resolve(handle)` per throw.
//
// Run with the fast throw ON in CVM: `-XX:+OmitStackTraceInFastThrow` (or
// CRATONVM_OMIT_STACK_TRACE_IN_FAST_THROW=1); HotSpot has it on by default.
// Arms: default (ON) vs CRATONVM_FAST_THROW_UNLOCKED_HANDOUT=0. A threaded row (4 threads
// throwing at once) is where the shared mutex was contended.
// Same `fact` lines on HotSpot 25 (counts and checksums only: whether a given throw is
// stackless depends on tiering on both VMs, so no trace shape is printed as a fact).
// Engagement: CRATONVM_DBG_FAST_THROW_HANDOUT=1 prints "[fast-throw-handout] unlocked n=<count>"
// at every power of two under ON, nothing under =0.
// Decides: `time npe-*` / `time aioobe-*` rows ON below =0 (the single-thread rows by the
// mutex's uncontended cost, the threaded row by more).
public class JtsA2 {
    static final int WARM = 40_000;
    static final int RUNS = 400_000;
    static volatile long sink;

    static final class Holder { int f; Holder(int f) { this.f = f; } }

    static final Holder[] HS = new Holder[16];
    static {
        for (int i = 0; i < HS.length; i++) HS[i] = (i % 4 == 3) ? null : new Holder(i);
    }
    static final int[] ARR = new int[8];

    static long npeKernel(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += HS[i & 15].f;
            } catch (NullPointerException e) {
                acc += 1000;
            }
        }
        return acc;
    }

    static long aioobeKernel(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            try {
                acc += ARR[i % 10];
            } catch (ArrayIndexOutOfBoundsException e) {
                acc += 7;
            }
        }
        return acc;
    }

    interface K { long run(int n); }

    static void row(String label, K k) {
        long acc = 0;
        for (int r = 0; r < 4; r++) acc += k.run(WARM / 4);
        long t0 = System.nanoTime();
        acc += k.run(RUNS);
        long t1 = System.nanoTime();
        sink += acc;
        System.out.println("time " + label + " " + ((t1 - t0) / RUNS) + " ns");
    }

    static void threadedRow(String label, K k, int threads) throws InterruptedException {
        long[] out = new long[threads];
        Thread[] ts = new Thread[threads];
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long acc = 0;
                for (int r = 0; r < 4; r++) acc += k.run(WARM / 4);
                acc += k.run(RUNS);
                out[id] = acc;
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        long t1 = System.nanoTime();
        long acc = 0;
        for (long v : out) acc += v;
        sink += acc;
        System.out.println("fact " + label + "-checksum " + acc);
        System.out.println("time " + label + " " + ((t1 - t0) / ((long) threads * (RUNS + WARM))) + " ns");
    }

    public static void main(String[] args) throws Exception {
        System.out.println("fact aioobe-checksum " + aioobeKernel(160_000));
        row("aioobe-caught", JtsA2::aioobeKernel);
        System.out.println("fact after " + aioobeKernel(16_000));
    }
}
