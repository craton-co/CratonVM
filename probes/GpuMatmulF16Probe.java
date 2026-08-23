import craton.gpu.AdmissionHint;
import craton.gpu.GpuArray;
import craton.gpu.GpuExecutor;
import craton.gpu.GpuKernel;

/**
 * Which half-precision matrix-vector kernel should a decode loop use?
 *
 * A decode step is entirely memory bound — it streams the whole weight
 * matrix and reuses nothing — so the only question that matters is what
 * fraction of the device's bandwidth each layout achieves. The probe
 * reports effective GB/s beside every timing, because ns per call means
 * nothing without the byte count it moved.
 *
 * Two layouts:
 *
 *   ROW  one thread per output row, reading its row contiguously.
 *        Consecutive threads are then a whole row apart, so a warp's
 *        32 loads are 32 separate memory transactions.
 *   COL  the same matrix transposed, so consecutive threads read
 *        consecutive words and a warp's loads coalesce.
 *
 * Both are checked against a CPU reference before either is timed.
 */
public class GpuMatmulF16Probe {

    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void matmulRow(int[] w, float[] x, float[] out) {
        int n = x.length;
        int half = n >> 1;
        int rows = out.length;
        for (int i = 0; i < rows; i++) {
            float sum = 0.0f;
            int base = i * half;
            for (int j = 0; j < half; j++) {
                int packed = w[base + j];
                int k = j << 1;
                sum += Float.float16ToFloat((short) packed) * x[k]
                     + Float.float16ToFloat((short) (packed >> 16)) * x[k + 1];
            }
            out[i] = sum;
        }
    }

    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void matmulCol(int[] wt, float[] x, float[] out) {
        int rows = out.length;
        int n = x.length;
        int halfRows = rows >> 1;
        for (int i = 0; i < rows; i++) {
            float sum = 0.0f;
            int wi = i >> 1;
            int sel = i & 1;
            for (int j = 0; j < n; j++) {
                int packed = wt[j * halfRows + wi];
                int bits = packed;
                if (sel != 0) {
                    bits = packed >> 16;
                }
                sum += Float.float16ToFloat((short) bits) * x[j];
            }
            out[i] = sum;
        }
    }

    /**
     * COL, split S ways along the summed dimension so the launch has S
     * times as many threads. A decode step's matrix is 2048 rows wide;
     * one thread per row is 64 warps, which on 30 SMs is two warps each
     * and no way to hide a global-load latency. Thread `t` owns output
     * row `t % rows` and chunk `t / rows`, so consecutive threads still
     * read consecutive words.
     *
     * The partial sums are combined by {@link #reducePartials}. This
     * changes the ORDER of the summation and therefore its last bits;
     * that is the price of the parallelism and the probe measures it.
     */
    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void matmulColSplit(int[] wt, float[] x, int chunks, float[] partial) {
        int total = partial.length;
        int n = x.length;
        int rows = total / chunks;
        int halfRows = rows >> 1;
        int per = n / chunks;
        for (int t = 0; t < total; t++) {
            int c = t / rows;
            int i = t - c * rows;
            int wi = i >> 1;
            int sel = i & 1;
            int j0 = c * per;
            int j1 = j0 + per;
            float sum = 0.0f;
            for (int j = j0; j < j1; j++) {
                int packed = wt[j * halfRows + wi];
                int bits = packed;
                if (sel != 0) {
                    bits = packed >> 16;
                }
                sum += Float.float16ToFloat((short) bits) * x[j];
            }
            partial[t] = sum;
        }
    }

    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void reducePartials(float[] partial, int chunks, float[] out) {
        int rows = out.length;
        for (int i = 0; i < rows; i++) {
            float s = 0.0f;
            for (int c = 0; c < chunks; c++) {
                s += partial[c * rows + i];
            }
            out[i] = s;
        }
    }

    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void transpose(int[] src, int rows, int cols, int[] out) {
        int words = out.length;
        int halfRows = rows >> 1;
        for (int k = 0; k < words; k++) {
            int j = k / halfRows;
            int wi = k - j * halfRows;
            int i0 = wi << 1;
            int flatA = i0 * cols + j;
            int wordA = src[flatA >> 1];
            int lo = wordA;
            if ((flatA & 1) != 0) {
                lo = wordA >> 16;
            }
            int flatB = flatA + cols;
            int wordB = src[flatB >> 1];
            int hi = wordB;
            if ((flatB & 1) != 0) {
                hi = wordB >> 16;
            }
            out[k] = (lo & 0xFFFF) | (hi << 16);
        }
    }

    private static final String K = "GpuMatmulF16Probe";

    public static void main(String[] args) throws Exception {
        int rows = Integer.parseInt(System.getProperty("rows", "2048"));
        int n = Integer.parseInt(System.getProperty("n", "2048"));
        int iters = Integer.parseInt(System.getProperty("iters", "30"));

        // Half-precision weights, row major, two lanes to a word.
        short[] halves = new short[rows * n];
        for (int i = 0; i < halves.length; i++) {
            halves[i] = Float.floatToFloat16(((i * 37 % 101) - 50) / 512.0f);
        }
        int[] w = new int[rows * n / 2];
        for (int k = 0; k < w.length; k++) {
            w[k] = (halves[2 * k] & 0xFFFF) | (halves[2 * k + 1] << 16);
        }
        float[] x = new float[n];
        for (int i = 0; i < n; i++) {
            x[i] = ((i * 13 % 71) - 35) / 256.0f;
        }

        float[] cpu = new float[rows];
        for (int i = 0; i < rows; i++) {
            float s = 0.0f;
            for (int j = 0; j < n; j += 2) {
                s += Float.float16ToFloat(halves[i * n + j]) * x[j]
                   + Float.float16ToFloat(halves[i * n + j + 1]) * x[j + 1];
            }
            cpu[i] = s;
        }

        long weightBytes = (long) rows * n * 2L;

        try (GpuExecutor exec = GpuExecutor.open()) {
            GpuArray<int[]> gw = GpuArray.wrap(w);
            GpuArray<int[]> gwt = GpuArray.wrap(new int[rows * n / 2]);
            GpuArray<float[]> gx = GpuArray.wrap(x);
            GpuArray<float[]> gout = GpuArray.wrap(new float[rows]);

            exec.submit(K, "matmulRow", "([I[F[F)V", gw, gx, gout).get();
            report("ROW ", gout, cpu, rows);

            exec.submit(K, "transpose", "([III[I)V",
                    gw, Integer.valueOf(rows), Integer.valueOf(n), gwt).get();
            exec.submit(K, "matmulCol", "([I[F[F)V", gwt, gx, gout).get();
            report("COL ", gout, cpu, rows);

            time(exec, "ROW", "matmulRow", gw, gx, gout, iters, weightBytes);
            time(exec, "COL", "matmulCol", gwt, gx, gout, iters, weightBytes);

            for (String s : System.getProperty("splits", "4,8,16,32,64").split(",")) {
                int chunks = Integer.parseInt(s.trim());
                if (n % chunks != 0) {
                    continue;
                }
                GpuArray<float[]> gpart = GpuArray.wrap(new float[rows * chunks]);
                Integer c = Integer.valueOf(chunks);
                try {
                    exec.submit(K, "matmulColSplit", "([I[FI[F)V", gwt, gx, c, gpart).get();
                } catch (Exception e) {
                    System.out.printf("SPLIT%d matmulColSplit FAILED: %s%n", chunks, e.getMessage());
                    gpart.close();
                    continue;
                }
                try {
                    exec.submit(K, "reducePartials", "([FI[F)V", gpart, c, gout).get();
                } catch (Exception e) {
                    System.out.printf("SPLIT%d reducePartials FAILED: %s%n", chunks, e.getMessage());
                    gpart.close();
                    continue;
                }
                report("SPLIT" + chunks + " ", gout, cpu, rows);

                for (int i = 0; i < 5; i++) {
                    exec.submit(K, "matmulColSplit", "([I[FI[F)V", gwt, gx, c, gpart).get();
                    exec.submit(K, "reducePartials", "([FI[F)V", gpart, c, gout).get();
                }
                long t0 = System.nanoTime();
                for (int i = 0; i < iters; i++) {
                    exec.submit(K, "matmulColSplit", "([I[FI[F)V", gwt, gx, c, gpart).get();
                    exec.submit(K, "reducePartials", "([FI[F)V", gpart, c, gout).get();
                }
                long ns = (System.nanoTime() - t0) / iters;
                System.out.printf("TIMING SPLIT%d ns_per_call=%d effective_GB_s=%.1f threads=%d%n",
                        chunks, ns, weightBytes / (double) ns, rows * chunks);
                gpart.close();
            }

            gout.close();
            gx.close();
            gwt.close();
            gw.close();
        }
    }

    private static void report(String tag, GpuArray<float[]> gout, float[] cpu, int rows) {
        float[] got = new float[rows];
        gout.toHost(got);
        int diffs = 0;
        double worst = 0.0;
        for (int i = 0; i < rows; i++) {
            if (Float.floatToRawIntBits(got[i]) != Float.floatToRawIntBits(cpu[i])) {
                diffs++;
                worst = Math.max(worst, Math.abs((double) got[i] - cpu[i]));
            }
        }
        System.out.printf("%s diff_lanes=%d worst_abs=%.3g cpu[0]=%a gpu[0]=%a%n",
                tag, diffs, worst, cpu[0], got[0]);
    }

    private static void time(GpuExecutor exec, String tag, String method,
                             GpuArray<int[]> w, GpuArray<float[]> x, GpuArray<float[]> out,
                             int iters, long weightBytes) throws Exception {
        for (int i = 0; i < 5; i++) {
            exec.submit(K, method, "([I[F[F)V", w, x, out).get();
        }
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            exec.submit(K, method, "([I[F[F)V", w, x, out).get();
        }
        long ns = (System.nanoTime() - t0) / iters;
        System.out.printf("TIMING %s ns_per_call=%d effective_GB_s=%.1f%n",
                tag, ns, weightBytes / (double) ns);
    }
}
