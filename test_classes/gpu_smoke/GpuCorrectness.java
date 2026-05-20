import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

public class GpuCorrectness {
    public static void main(String[] args) throws Exception {
        int n = 1024;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = i * 2; }
        // out left at zero
        System.out.println("before: out[0]=" + out[0] + " out[n-1]=" + out[n-1]);

        try (GpuExecutor exec = GpuExecutor.open()) {
            GpuFuture<Void> f = exec.submit("EligibleVectorAdd", "vectorAdd", "([I[I[I)V", a, b, out);
            f.get();
        }
        System.out.println("after-gpu: out[0]=" + out[0] + " out[n-1]=" + out[n-1]);
        // Expected: out[0]=0+0=0, out[n-1]=1023+2046=3069
        boolean ok = (out[0] == 0) && (out[n-1] == 3069);
        System.out.println("correctness=" + (ok ? "OK" : "FAIL"));
        System.exit(ok ? 0 : 1);
    }
}
