import craton.gpu.GpuKernel;

/**
 * The transfer floor for an `int[]`-out kernel: the same launch shape and
 * the same number of output bytes as the ray tracer, with essentially no
 * arithmetic per element.
 *
 * Subtracting this from a compute-heavy kernel of the same output size
 * separates the two halves of the per-pixel cost — how much is the device
 * doing arithmetic, and how much is moving 4 bytes per element back across
 * PCIe into the Java heap. Neither is visible on its own from a single
 * wall-clock number, and the ray tracer's per-pixel slope (0.911 ns) is
 * consistent with either being dominant.
 *
 *   GpuTransferFloor <n> <iters>
 */
public class GpuTransferFloor {

    @GpuKernel
    public static void fill(int[] out) {
        int n = out.length;
        for (int i = 0; i < n; i++) {
            out[i] = i * 3;
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2764800;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 30;
        int[] out = new int[n];

        fill(out); // warm-up: analyze + lower + PTX cache miss

        long total = 0, best = Long.MAX_VALUE;
        for (int it = 0; it < iters; it++) {
            long t0 = System.nanoTime();
            fill(out);
            long dt = System.nanoTime() - t0;
            total += dt;
            if (dt < best) best = dt;
        }
        long checksum = 0;
        for (int v : out) checksum += v;
        System.out.println("TRANSFERFLOOR n=" + n
                + " mean_ms=" + (total / (double) iters / 1e6)
                + " best_ms=" + (best / 1e6)
                + " ns_per_elem=" + (best / (double) n)
                + " checksum=" + checksum);
    }
}
