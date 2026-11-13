// Lane L1 bench (interpreter round i1 wave 17): the safepoint poll's slow path
// now takes a second argument, the polling body's compile id
// (`i15-L3-proposal-poll-slow-path-names-its-compiled-body`). The fast path
// (flag clear) is unchanged; the slow path gains one `MOV` and, with no agent,
// the VM still answers the verdict from a few per-VM flags without looking
// the body up.
//
// What to measure: this bench should time the same as the previous build.
// Each round runs allocation-heavy counted loops in methods compiled at method
// entry and by OSR, so the collector stops the world often and every running
// loop's back-edge poll takes its slow path at each stop. A regression here
// means the slow path (or the verdict it now computes) got slower.
//
// No setup; run under --compatible. stdout: a deterministic checksum
// (HotSpot 25 prints the same line); stderr: per-round and median times.
public class L1W17PollSlowPathBench {
    static final int ROUNDS = 9;
    static final int CALLS = 200;
    static final int N = 20_000;

    /// A counted loop that allocates on every iteration: compiled at method
    /// entry after a few calls, and polled at every back edge.
    static long churn(int seed) {
        long acc = seed;
        for (int i = 0; i < N; i++) {
            int[] box = new int[4];
            box[i & 3] = i ^ seed;
            acc += box[i & 3] + box.length;
        }
        return acc;
    }

    /// One long-running loop, entered through OSR.
    static long churnLong(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            Integer boxed = Integer.valueOf(i * 31 + 7);
            acc += boxed.hashCode() & 0xFF;
        }
        return acc;
    }

    public static void main(String[] args) {
        long checksum = 0;
        long[] times = new long[ROUNDS];
        for (int r = 0; r < ROUNDS; r++) {
            long t0 = System.nanoTime();
            long round = 0;
            for (int c = 0; c < CALLS; c++) {
                round += churn(c);
            }
            round += churnLong(2_000_000);
            times[r] = System.nanoTime() - t0;
            checksum = checksum * 31 + round;
            System.err.printf("round %d: %.2f ms%n", r, times[r] / 1e6);
        }
        java.util.Arrays.sort(times);
        System.err.printf("median: %.2f ms%n", times[ROUNDS / 2] / 1e6);
        System.out.println("checksum " + checksum);
    }
}
