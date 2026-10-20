package gpu.annotations;
import craton.gpu.GpuKernel;
import craton.gpu.AdmissionHint;
public class AdmitMathSqrt {
    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void rootArray(double[] src, double[] dst) {
        int n = src.length;
        for (int i = 0; i < n; i++) {
            dst[i] = Math.sqrt(src[i]);
        }
    }
}
