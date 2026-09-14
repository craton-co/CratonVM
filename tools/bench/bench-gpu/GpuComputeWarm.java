/**
 * Warm harness for GpuCompute.heavy.
 *
 * `GpuCompute` times ONE call with no warm-up, so under any JIT — and
 * under GPU offload, where the first call pays analyze + lower + ptxas +
 * module load — its number is a cold number. The README's row is warm
 * ("N = 2^24, warm, full H2D+kernel+D2H round-trip"), so reproducing it
 * needs this: call the same kernel `reps` times and report the best.
 *
 * Usage: GpuComputeWarm [n] [reps]
 */
public class GpuComputeWarm {
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 24);
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
            b[i] = 1 + (i % 13);
        }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            GpuCompute.heavy(a, b, out);
            long ms = (System.nanoTime() - t0) / 1_000_000L;
            if (ms < best) best = ms;
        }
        long checksum = 0;
        for (int i = 0; i < n; i++) checksum += out[i];
        System.out.println("n=" + n);
        System.out.println("heavy_ms=" + best);
        System.out.println("COMPUTE_CHECKSUM=" + checksum);
    }
}
