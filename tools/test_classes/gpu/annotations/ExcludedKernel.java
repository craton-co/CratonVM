package gpu.annotations;
import craton.gpu.GpuExclude;
public class ExcludedKernel {
    @GpuExclude(reason = "branchy; CPU is faster")
    public static void vectorAdd(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
}
