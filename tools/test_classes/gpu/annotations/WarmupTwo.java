package gpu.annotations;
import craton.gpu.EnableGpuAsync;
import craton.gpu.GpuKernel;
@EnableGpuAsync(warmup = 2)
public class WarmupTwo {
    @GpuKernel
    public static void k1(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }

    @GpuKernel
    public static void k2(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] * b[i];
        }
    }

    @GpuKernel
    public static void k3(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] - b[i];
        }
    }
}
