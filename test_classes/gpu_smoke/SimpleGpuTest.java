import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

public class SimpleGpuTest {
    public static void main(String[] args) throws Exception {
        int n = 1024;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = i * 2; }

        System.out.println("step1: arrays initialized");

        // CPU run
        EligibleVectorAdd.vectorAdd(a, b, out);
        System.out.println("step2: CPU out[0]=" + out[0] + " out[" + (n-1) + "]=" + out[n-1]);

        // GPU run
        try (GpuExecutor exec = GpuExecutor.open()) {
            System.out.println("step3: GPU opened");
            GpuFuture<Void> f = exec.submit(
                "EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
            System.out.println("step4: submitted");
            f.get();
            System.out.println("step5: GPU out[0]=" + out[0] + " out[" + (n-1) + "]=" + out[n-1]);
        }
        System.out.println("DONE");
    }
}
