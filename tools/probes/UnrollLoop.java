/**
 * The partial unroller's own shape, in `tools/tier-ab/flag-ab.sh`'s format.
 *
 * `bench/C2PartialUnrollProbe.java` is the same loop and prints its own
 * timing line; this one prints `acc=... ms=...`, which is what the A/B
 * harness greps for, so the transform can be measured against a CONTROL arm
 * rather than against a second block of runs. That distinction is the whole
 * reason the first partial-unroll measurement could only report 0.98 against
 * a +-8% spread.
 *
 * `sum` is the runtime-bound counted loop with a pure integer body -- the
 * population the unroll census found is EVERY counted loop in both benchmark
 * suites, and the one full unrolling can never serve.
 *
 * `sumArr` is the same shape with an array element in the body. It is the
 * reach question rather than the speed question: the shared side-effect scan
 * refuses a loop containing an `ArrayLoad`, so this arm is what says whether
 * widening that scan for the partial path is worth doing.
 */
public class UnrollLoop {

    static int sum(int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            a += i ^ (a >>> 3);
        }
        return a;
    }

    static int sumArr(int[] v, int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            a += v[i] ^ (a >>> 3);
        }
        return a;
    }

    public static void main(String[] args) {
        int reps = Integer.getInteger("probe.reps", 400000);
        int n = Integer.getInteger("probe.n", 1001);
        boolean arr = Boolean.getBoolean("probe.arr");
        int[] v = new int[n];
        for (int i = 0; i < n; i++) v[i] = i;

        long acc = 0;
        // Warm past the optimizing tier's invocation threshold (20,000) on
        // both doors before the clock starts.
        for (int r = 0; r < 30000; r++) acc += arr ? sumArr(v, 24) : sum(24);

        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) acc += arr ? sumArr(v, n) : sum(n);
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }
}
