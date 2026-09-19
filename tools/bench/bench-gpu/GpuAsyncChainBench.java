import craton.gpu.GpuArray;
import craton.gpu.GpuExecutor;

/**
 * A chain of small kernels on one executor, awaited once at the end —
 * the shape of an inference decode step, and the shape a per-launch host
 * callback hurts most.
 *
 * Every launch here goes through the explicit async API
 * ({@code dispatchNamedHandle}), which is the path the VM's completion
 * reaper watches. Until 2026-09-02 that path registered a
 * {@code cuLaunchHostFunc} per launch; a host function blocks every launch
 * queued behind it on its stream until it has run, so a chain of N kernels
 * paid N driver-thread round trips. The reaper now polls the completion
 * event instead. {@code CRATONVM_GPU_HOST_CALLBACK=1} restores the callback,
 * which makes this bench a same-binary A/B:
 *
 * <pre>
 *   cratonvm --gpu -cp bench-gpu GpuAsyncChainBench [n] [launches] [rounds]
 *   CRATONVM_GPU_HOST_CALLBACK=1 cratonvm --gpu -cp bench-gpu GpuAsyncChainBench ...
 * </pre>
 *
 * Reports, per round, {@code submit_ms} (host time to issue the chain),
 * {@code total_ms} (issue plus the one await) and the per-launch mean of
 * each; the best round is what to compare. The arrays are resident
 * {@code GpuArray}s so no per-launch host writeback dilutes the number, and
 * the kernel is tiny (a vector add over {@code n} ints) so the device is
 * mostly waiting on the host — the regime in which dispatch cost is the
 * whole story.
 */
public class GpuAsyncChainBench {
    /** The kernel: eligible, element-wise, three arrays. */
    static void vecAdd(int[] a, int[] b, int[] out) {
        int n = out.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 65_536;
        int launches = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 5;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
            b[i] = 3 * i + 1;
        }
        System.out.println("n=" + n + " launches=" + launches + " rounds=" + rounds);
        long[] chain = new long[launches];
        try (GpuExecutor exec = GpuExecutor.open()) {
            GpuArray<int[]> ga = GpuArray.wrap(a);
            GpuArray<int[]> gb = GpuArray.wrap(b);
            GpuArray<int[]> gout = GpuArray.wrap(out);
            // Warm-up: analyze, lower, load the module, upload the arrays.
            long w = exec.dispatchNamedHandle(
                    "GpuAsyncChainBench", "vecAdd", "([I[I[I)V", new Object[] {ga, gb, gout});
            exec.awaitSubmission(w);
            exec.releaseSubmission(w);

            double bestSubmit = Double.MAX_VALUE;
            double bestTotal = Double.MAX_VALUE;
            for (int r = 0; r < rounds; r++) {
                long t0 = System.nanoTime();
                for (int i = 0; i < launches; i++) {
                    chain[i] = exec.dispatchNamedHandle(
                            "GpuAsyncChainBench", "vecAdd", "([I[I[I)V", new Object[] {ga, gb, gout});
                }
                long t1 = System.nanoTime();
                exec.awaitSubmission(chain[launches - 1]);
                long t2 = System.nanoTime();
                for (long h : chain) {
                    exec.releaseSubmission(h);
                }
                double submitMs = (t1 - t0) / 1e6;
                double totalMs = (t2 - t0) / 1e6;
                bestSubmit = Math.min(bestSubmit, submitMs);
                bestTotal = Math.min(bestTotal, totalMs);
                System.out.println("round=" + r
                        + " submit_ms=" + fmt(submitMs)
                        + " total_ms=" + fmt(totalMs)
                        + " submit_us_per_launch=" + fmt(submitMs * 1000.0 / launches)
                        + " total_us_per_launch=" + fmt(totalMs * 1000.0 / launches));
            }
            int[] check = new int[n];
            gout.toHost(check);
            long checksum = 0;
            for (int v : check) checksum += v;
            long expect = 0;
            for (int i = 0; i < n; i++) expect += a[i] + b[i];
            System.out.println("ASYNCCHAIN best_submit_ms=" + fmt(bestSubmit)
                    + " best_total_ms=" + fmt(bestTotal)
                    + " best_total_us_per_launch=" + fmt(bestTotal * 1000.0 / launches)
                    + " checksum=" + checksum
                    + " expected=" + expect
                    + " ok=" + (checksum == expect));
            ga.close();
            gb.close();
            gout.close();
        }
    }

    static String fmt(double v) {
        return String.format(java.util.Locale.ROOT, "%.3f", v);
    }
}
