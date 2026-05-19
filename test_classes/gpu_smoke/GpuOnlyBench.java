import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

public class GpuOnlyBench {
    public static void main(String[] args) throws Exception {
        int n = (args.length > 0) ? Integer.parseInt(args[0]) : 1048576;
        int iters = (args.length > 1) ? Integer.parseInt(args[1]) : 5;
        int warmup = (args.length > 2) ? Integer.parseInt(args[2]) : 2;

        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = (i * 7) % 1000; }

        System.out.println("n=" + n);

        // GPU first (no CPU baseline that triggers any JIT)
        try (GpuExecutor exec = GpuExecutor.open()) {
            for (int w = 0; w < warmup; w++) {
                GpuFuture<Void> f = exec.submit("EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
                f.get();
            }
            long gpuBest = -1; long gpuSum = 0;
            for (int it = 0; it < iters; it++) {
                long t0 = System.nanoTime();
                GpuFuture<Void> f = exec.submit("EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
                f.get();
                long dt = System.nanoTime() - t0;
                gpuSum = gpuSum + dt;
                if (gpuBest < 0 || dt < gpuBest) gpuBest = dt;
            }
            int gpu0 = out[0], gpuN = out[n - 1];
            System.out.println("GPU best_ns=" + gpuBest + " mean_ns=" + (gpuSum / iters)
                + " out[0]=" + gpu0 + " out[n-1]=" + gpuN);
        }

        // ONE CPU verification call (avoid JIT trigger from looped calls)
        int[] outRef = new int[n];
        EligibleVectorAdd.vectorAdd(a, b, outRef);
        int cpu0 = outRef[0], cpuN = outRef[n - 1];
        System.out.println("CPU one-shot out[0]=" + cpu0 + " out[n-1]=" + cpuN);
        boolean ok = (out[0] == cpu0) && (out[n - 1] == cpuN);
        System.out.println("correctness=" + (ok ? "OK" : "FAIL"));
        System.exit(ok ? 0 : 1);
    }
}
