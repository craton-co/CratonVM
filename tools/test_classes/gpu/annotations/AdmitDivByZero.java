package gpu.annotations;
import craton.gpu.GpuKernel;
import craton.gpu.AdmissionHint;
public class AdmitDivByZero {
    @GpuKernel(admit = AdmissionHint.ALLOW_DIV_BY_ZERO)
    public static void elementwiseDiv(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] / b[i];
        }
    }
}
