/**
 * `fib` alone, in `tools/tier-ab/flag-ab.sh`'s `acc=... ms=...` format.
 *
 * `CratonBench`'s fib phase is the largest gap on any row of that benchmark
 * that is PURE COMPILATION -- no allocation, no collections, no collector
 * involvement, one arithmetic expression and two calls. This probe is that
 * phase and nothing else, so the two tiers and the individual call-path flags
 * can be A/B'd against a control arm.
 *
 * `probe.n` is the argument, defaulting to 32 rather than CratonBench's 44 so
 * one run is ~100 ms rather than ~12 s -- an A/B with a control arm wants many
 * short runs, not few long ones. The call count is ~2*fib(n+1), so n=32 is
 * about 7 million calls and n=44 about 2.3 billion.
 */
public class FibCall {

    static int fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 32);
        int reps = Integer.getInteger("probe.reps", 12);

        long acc = 0;
        // Warm both doors -- the invocation counter reaches the optimizing
        // tier's threshold in the first few thousand recursive calls.
        for (int r = 0; r < 3; r++) acc += fib(20);

        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) acc += fib(n);
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }
}
