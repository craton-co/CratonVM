import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

public class GpuBenchNoTime {
    public static void main(String[] args) throws Exception {
        int n = (args.length > 0) ? Integer.parseInt(args[0]) : 1048576;
        int iters = (args.length > 1) ? Integer.parseInt(args[1]) : 5;
        int warmup = (args.length > 2) ? Integer.parseInt(args[2]) : 2;

        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = (i * 7) % 1000; }

        System.out.println("n=" + n);

        // CPU run (no timing)
        for (int it = 0; it < iters + warmup; it++) {
            EligibleVectorAdd.vectorAdd(a, b, out);
        }
        int cpuRef0 = out[0];
        int cpuRefN = out[n - 1];
        System.out.println("CPU out[0]=" + cpuRef0 + " out[n-1]=" + cpuRefN);

        // GPU run (no timing)
        try (GpuExecutor exec = GpuExecutor.open()) {
            for (int it = 0; it < iters + warmup; it++) {
                GpuFuture<Void> f = exec.submit(
                    "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
                f.get();
            }
            int gpuRef0 = out[0];
            int gpuRefN = out[n - 1];
            System.out.println("GPU out[0]=" + gpuRef0 + " out[n-1]=" + gpuRefN);
            boolean ok = (gpuRef0 == cpuRef0) && (gpuRefN == cpuRefN);
            System.out.println("correctness=" + (ok ? "OK" : "FAIL"));
            System.exit(ok ? 0 : 1);
        }
    }
}
