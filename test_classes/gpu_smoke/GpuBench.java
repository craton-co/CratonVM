import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

public class GpuBench {
    public static void main(String[] args) throws Exception {
        int n = (args.length > 0) ? Integer.parseInt(args[0]) : 4194304;
        int iters = (args.length > 1) ? Integer.parseInt(args[1]) : 5;
        int warmup = (args.length > 2) ? Integer.parseInt(args[2]) : 2;

        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = (i * 7) % 1000; }

        System.out.println("n=" + n + " iters=" + iters + " warmup=" + warmup);

        // CPU warmup + run
        for (int w = 0; w < warmup; w++) EligibleVectorAdd.vectorAdd(a, b, out);
        long cpuBest = -1;
        long cpuSum = 0;
        for (int it = 0; it < iters; it++) {
            long t0 = System.nanoTime();
            EligibleVectorAdd.vectorAdd(a, b, out);
            long dt = System.nanoTime() - t0;
            cpuSum = cpuSum + dt;
            if (cpuBest < 0 || dt < cpuBest) cpuBest = dt;
        }
        int cpuRef0 = out[0];
        int cpuRefN = out[n - 1];
        System.out.println("CPU best_ns=" + cpuBest + " mean_ns=" + (cpuSum / iters)
            + " out[0]=" + cpuRef0 + " out[n-1]=" + cpuRefN);

        // GPU warmup + run
        try (GpuExecutor exec = GpuExecutor.open()) {
            for (int w = 0; w < warmup; w++) {
                GpuFuture<Void> f = exec.submit(
                    "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
                f.get();
            }
            long gpuBest = -1;
            long gpuSum = 0;
            for (int it = 0; it < iters; it++) {
                long t0 = System.nanoTime();
                GpuFuture<Void> f = exec.submit(
                    "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
                f.get();
                long dt = System.nanoTime() - t0;
                gpuSum = gpuSum + dt;
                if (gpuBest < 0 || dt < gpuBest) gpuBest = dt;
            }
            int gpuRef0 = out[0];
            int gpuRefN = out[n - 1];
            System.out.println("GPU best_ns=" + gpuBest + " mean_ns=" + (gpuSum / iters)
                + " out[0]=" + gpuRef0 + " out[n-1]=" + gpuRefN);

            boolean ok = (gpuRef0 == cpuRef0) && (gpuRefN == cpuRefN);
            System.out.println("correctness=" + (ok ? "OK" : "FAIL"));
            System.out.println("speedup_best=" + ((double)cpuBest / (double)gpuBest));
            System.out.println("speedup_mean=" + ((double)cpuSum / (double)gpuSum));
            System.exit(ok ? 0 : 1);
        }
    }
}
