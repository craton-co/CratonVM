package gpu.annotations;
import craton.gpu.GpuKernel;
import craton.gpu.GpuExclude;
public class ExcludedAndKernel {
    @GpuKernel
    @GpuExclude(reason = "overrides @GpuKernel")
    public static void vectorAdd(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
}
