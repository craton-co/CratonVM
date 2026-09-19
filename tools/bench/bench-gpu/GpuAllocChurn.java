import craton.gpu.GpuKernel;

/**
 * The one place a difference between the two driver backends is
 * MECHANISTICALLY predicted, rather than fished for.
 *
 * `backend_cuda` keeps an `AllocPool` and retires device allocations into
 * it; `backend_oxide` has no pool at all, so `set_retire_to_pool` is a
 * no-op and every buffer is a fresh `cuMemAlloc` freed at drop. Nothing in
 * `GpuTransferFloor` can see that: it reuses ONE output array, so the
 * residency cache serves the same device buffer every call and the
 * allocator is never entered.
 *
 * This allocates a FRESH `out[]` per iteration. A new Java array is a
 * residency-cache miss, so each call must allocate a new device buffer --
 * which is exactly the path the pool exists to shortcut.
 *
 * Predicted direction: cudarc (pooled) faster than oxide (unpooled). A
 * result in the other direction, or none at all, says the pool is not
 * paying for itself on this shape -- which is worth knowing too.
 *
 *   GpuAllocChurn <n> <iters>
 */
public class GpuAllocChurn {

    @GpuKernel
    public static void fill(int[] out) {
        int n = out.length;
        for (int i = 0; i < n; i++) {
            out[i] = i * 3;
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1048576;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 50;

        // Warm-up: analyze + lower + PTX cache miss, on a throwaway array.
        fill(new int[n]);

        long total = 0, best = Long.MAX_VALUE;
        long checksum = 0;
        for (int it = 0; it < iters; it++) {
            // A FRESH array every iteration. This is the whole point: the
            // residency cache keys on the array, so this misses and forces
            // a device-side allocation per call.
            int[] out = new int[n];
            long t0 = System.nanoTime();
            fill(out);
            long dt = System.nanoTime() - t0;
            total += dt;
            if (dt < best) best = dt;
            // Touch the result so nothing above can be optimised away, and
            // so a backend that returned a stale or unwritten buffer shows
            // up as a checksum mismatch rather than as a faster arm.
            checksum += out[0] + out[n / 2] + out[n - 1];
        }
        System.out.println("ALLOCCHURN n=" + n
                + " iters=" + iters
                + " mean_ms=" + (total / (double) iters / 1e6)
                + " best_ms=" + (best / 1e6)
                + " checksum=" + checksum);
    }
}
