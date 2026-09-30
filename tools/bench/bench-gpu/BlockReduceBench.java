/**
 * A reduction with as little arithmetic per element as the shape allows,
 * so the cost of the atomics is not buried under it.
 *
 * `GpuDotBench` multiplies and then adds the product 64 times per
 * element: it is compute-bound, it finishes 2^24 elements in 2 ms, and
 * both arms of the block-reduction switch read `dot_ms=2` because that
 * is the resolution of the clock, not because nothing changed. This one
 * does a single widening add per element, which is the regime where the
 * number of `red.global.add`s into the one accumulator cell matters:
 *
 *   per thread   (before 2026-09-02)   n atomics
 *   per warp     (warp fold)           n/32
 *   per block    (block fold)          n/256 at the usual block size
 *
 * Usage: cratonvm --gpu -cp bench-gpu BlockReduceBench [n] [reps]
 *   CRATONVM_GPU_BLOCK_REDUCE=0 is the control arm.
 */
public class BlockReduceBench {
    /** Eligible reduction: one counted loop, scalar accumulator, no store. */
    static long sum(int[] a) {
        long s = 0;
        int n = a.length;
        for (int i = 0; i < n; i++) {
            s += a[i];
        }
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 26);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 9;
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i & 1023;
        }
        long want = 0;
        for (int i = 0; i < n; i++) {
            want += a[i];
        }
        // Warm-up: analyze, lower, load the module, upload the array.
        long got = sum(a);

        double best = Double.MAX_VALUE;
        double total = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            got = sum(a);
            double ms = (System.nanoTime() - t0) / 1e6;
            best = Math.min(best, ms);
            total += ms;
        }
        System.out.println("BLOCKREDUCE n=" + n + " reps=" + reps
                + " best_ms=" + fmt(best)
                + " mean_ms=" + fmt(total / reps)
                + " checksum=" + got
                + " expected=" + want
                + " ok=" + (got == want));
    }

    static String fmt(double v) {
        return String.format(java.util.Locale.ROOT, "%.4f", v);
    }
}
