import craton.gpu.GpuArray;
import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

/**
 * End-to-end probe for the outer-parallel / sequential-inner-loop
 * lowering: a matrix-vector product with device-resident operands.
 *
 * Three things are being asked, in order, and each one is printed with
 * the evidence for it rather than as a verdict:
 *
 *   1. VALUES  — does the GPU kernel produce exactly what the same
 *      Java source produces on the CPU? Reported as the number of
 *      differing lanes and the worst absolute difference. Exact
 *      agreement is expected: the emitter spells every float op with
 *      an explicit rounding mode, so ptxas cannot contract a*b + c.
 *   2. CHAINING — does a second kernel reading the first's output see
 *      the DEVICE copy, without a round trip through the host?
 *   3. THROUGHPUT — ns per call once the weights are resident, which
 *      is the number that decides whether an inference loop built out
 *      of these is worth having.
 */
public class GpuMatmulProbe {

    public static void matmul(float[] w, float[] x, float[] out) {
        int n = x.length;
        int rows = out.length;
        for (int i = 0; i < rows; i++) {
            float sum = 0.0f;
            for (int j = 0; j < n; j++) {
                sum += w[i * n + j] * x[j];
            }
            out[i] = sum;
        }
    }

    /** Second kernel in the chain: y[i] = a * z[i]. */
    public static void scale(float[] z, float a, float[] outv) {
        int n = outv.length;
        for (int i = 0; i < n; i++) {
            outv[i] = z[i] * a;
        }
    }

    public static void main(String[] args) throws Exception {
        int rows = Integer.parseInt(System.getProperty("rows", "2048"));
        int n = Integer.parseInt(System.getProperty("n", "2048"));
        int iters = Integer.parseInt(System.getProperty("iters", "50"));

        float[] w = new float[rows * n];
        float[] x = new float[n];
        // Small magnitudes so the sum stays well inside float range and
        // the CPU and GPU orders of accumulation cannot diverge.
        for (int i = 0; i < w.length; i++) {
            w[i] = ((i * 37 % 101) - 50) / 512.0f;
        }
        for (int i = 0; i < n; i++) {
            x[i] = ((i * 13 % 71) - 35) / 256.0f;
        }

        float[] cpu = new float[rows];
        long t0 = System.nanoTime();
        matmul(w, x, cpu);
        long cpuNs = System.nanoTime() - t0;

        try (GpuExecutor exec = GpuExecutor.open()) {
            GpuArray<float[]> gw = GpuArray.wrap(w);
            GpuArray<float[]> gx = GpuArray.wrap(x);
            GpuArray<float[]> gout = GpuArray.wrap(new float[rows]);

            GpuFuture<Void> f = exec.submit(
                "GpuMatmulProbe", "matmul", "([F[F[F)V", gw, gx, gout);
            f.get();

            float[] gpu = new float[rows];
            gout.toHost(gpu);

            int diffs = 0;
            float worst = 0.0f;
            for (int i = 0; i < rows; i++) {
                if (Float.floatToRawIntBits(gpu[i]) != Float.floatToRawIntBits(cpu[i])) {
                    diffs++;
                    float d = Math.abs(gpu[i] - cpu[i]);
                    if (d > worst) worst = d;
                }
            }
            System.out.printf("VALUES rows=%d n=%d diff_lanes=%d worst_abs=%g cpu[0]=%a gpu[0]=%a%n",
                    rows, n, diffs, worst, cpu[0], gpu[0]);

            // 2. Chaining: scale the resident output by 2 without ever
            //    sending it back to the host in between.
            GpuArray<float[]> gscaled = GpuArray.wrap(new float[rows]);
            exec.submit("GpuMatmulProbe", "scale", "([FF[F)V",
                    gout, Float.valueOf(2.0f), gscaled).get();
            float[] scaled = new float[rows];
            gscaled.toHost(scaled);
            int chainBad = 0;
            for (int i = 0; i < rows; i++) {
                if (Float.floatToRawIntBits(scaled[i]) != Float.floatToRawIntBits(cpu[i] * 2.0f)) {
                    chainBad++;
                }
            }
            System.out.printf("CHAIN  bad_lanes=%d scaled[0]=%a expected[0]=%a%n",
                    chainBad, scaled[0], cpu[0] * 2.0f);

            // 3. Throughput with the weights already resident.
            for (int i = 0; i < 5; i++) {
                exec.submit("GpuMatmulProbe", "matmul", "([F[F[F)V", gw, gx, gout).get();
            }
            long t1 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                exec.submit("GpuMatmulProbe", "matmul", "([F[F[F)V", gw, gx, gout).get();
            }
            long gpuNs = (System.nanoTime() - t1) / iters;
            System.out.printf("TIMING gpu_ns_per_call=%d cpu_ns_first_call=%d macs=%d%n",
                    gpuNs, cpuNs, (long) rows * n);

            gscaled.close();
            gout.close();
            gx.close();
            gw.close();
        }
    }
}
