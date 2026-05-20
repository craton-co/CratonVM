import craton.gpu.GpuExecutor;
import craton.gpu.GpuFuture;

public class GpuTestLoop {
    public static void main(String[] args) throws Exception {
        int n = 4096;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = i * 2; }

        System.out.println("step1: init done");

        // ONE CPU call
        EligibleVectorAdd.vectorAdd(a, b, out);
        System.out.println("step2: CPU once out[0]=" + out[0] + " out[n-1]=" + out[n-1]);

        // CPU loop of 3
        for (int k = 0; k < 3; k++) {
            EligibleVectorAdd.vectorAdd(a, b, out);
        }
        System.out.println("step3: CPU loop done out[0]=" + out[0]);

        System.out.println("step4: DONE");
    }
}
