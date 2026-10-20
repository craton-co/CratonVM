/**
 * Does a compiled frame on a PEER thread block relocation, and if so on which
 * obligation?
 *
 * `frame_cov` (per-frame map location) reads clean on every single-threaded
 * workload: `no_map=0 incomplete=0 ok=N`. The refusal that matters is the
 * CROSS-THREAD one -- `incomplete_reason::CROSS_THREAD_JIT_PEER`, raised when a
 * thread other than the collection initiator is inside compiled code -- and it
 * needs several threads that are simultaneously JIT-hot and allocating.
 *
 * Read with:
 *   CRATONVM_DBG_JIT_ROOTSCAN=1 cratonvm --verbose:gc ... OopMapPeerCoverage
 * and look at `xt_cov=(accepted refused deposits)` beside
 * `frame_cov=(no_slot misaligned no_map incomplete ok)`.
 */
public class OopMapPeerCoverage {
    static final int THREADS = 6;
    static final int ROUNDS = 6000;

    /** Hot enough to compile, and it keeps live references in frame slots
     *  across the allocation that can collect. */
    static Object[] churn(Object[] carry, int n) {
        Object[] out = new Object[8];
        for (int i = 0; i < 8; i++) {
            out[i] = new int[(n % 512) + 64];
        }
        // `carry` stays live across the allocations above: a reference in a
        // frame slot at a safepoint, which is what the oop map must name. It is
        // NOT stored into `out` -- an earlier version did, which chains every
        // generation into a list and retains the whole run (~360 MB at these
        // settings). The probe then OOM'd, its threads died silently, and the
        // partial sums read as a relocation corruption. The instrument has to
        // produce garbage, not a leak.
        out[7] = new int[1];
        return out;
    }

    public static void main(String[] args) throws Exception {
        Thread[] ts = new Thread[THREADS];
        final long[] sums = new long[THREADS];
        for (int t = 0; t < THREADS; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                Object[] carry = new Object[4];
                long s = 0;
                for (int r = 0; r < ROUNDS; r++) {
                    carry = churn(carry, r + id);
                    s += ((int[]) carry[1]).length;
                    // Every generation but the newest is garbage from here.
                }
                sums[id] = s;
            }, "churn-" + t);
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        long total = 0;
        for (long s : sums) total += s;
        System.out.println("PASS OopMapPeerCoverage total=" + total);
    }
}
