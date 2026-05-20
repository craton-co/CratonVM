import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

public class GpuVecAddBench {
    public static void main(String[] args) throws Exception {
        int n = (args.length > 0) ? Integer.parseInt(args[0]) : 1048576;
        int iters = (args.length > 1) ? Integer.parseInt(args[1]) : 5;
        int warmup = (args.length > 2) ? Integer.parseInt(args[2]) : 2;

        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
            b[i] = (i * 7) % 1000;
        }

        System.out.println("n=" + n + " iters=" + iters + " warmup=" + warmup);

        // CPU baseline (no GPU)
        long cpuTotal = 0;
        long cpuMin = Long.MAX_VALUE;
        for (int it = 0; it < iters + warmup; it++) {
            long t0 = System.nanoTime();
            EligibleVectorAdd.vectorAdd(a, b, out);
            long t1 = System.nanoTime();
            if (it >= warmup) {
                long dt = t1 - t0;
                cpuTotal += dt;
                if (dt < cpuMin) cpuMin = dt;
            }
        }
        int cpuRef0 = out[0];
        int cpuRefN = out[n - 1];
        System.out.println("CPU: min_ns=" + cpuMin + " mean_ns=" + (cpuTotal / iters)
            + " out[0]=" + cpuRef0 + " out[n-1]=" + cpuRefN);

        // GPU path via explicit submit
        try (GpuExecutor exec = GpuExecutor.open()) {
            long gpuTotal = 0;
            long gpuMin = Long.MAX_VALUE;
            for (int it = 0; it < iters + warmup; it++) {
                long t0 = System.nanoTime();
                GpuFuture<Void> f = exec.submit(
                    "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
                f.get();
                long t1 = System.nanoTime();
                if (it >= warmup) {
                    long dt = t1 - t0;
                    gpuTotal += dt;
                    if (dt < gpuMin) gpuMin = dt;
                }
            }
            int gpuRef0 = out[0];
            int gpuRefN = out[n - 1];
            System.out.println("GPU: min_ns=" + gpuMin + " mean_ns=" + (gpuTotal / iters)
                + " out[0]=" + gpuRef0 + " out[n-1]=" + gpuRefN);

            boolean correct = (gpuRef0 == cpuRef0) && (gpuRefN == cpuRefN);
            System.out.println("correctness=" + (correct ? "OK" : "FAIL"));
            double speedup = (double) cpuMin / (double) gpuMin;
            System.out.println("speedup_gpu_over_cpu=" + speedup + "x");
            System.exit(correct ? 0 : 1);
        }
    }
}
