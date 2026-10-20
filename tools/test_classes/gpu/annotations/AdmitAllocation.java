package gpu.annotations;
import craton.gpu.GpuKernel;
import craton.gpu.AdmissionHint;
public class AdmitAllocation {
    @GpuKernel(admit = AdmissionHint.ALLOW_ALLOCATION)
    public static int[] mapSquare(int[] src) {
        int n = src.length;
        int[] dst = new int[n];
        for (int i = 0; i < n; i++) {
            dst[i] = src[i] * src[i];
        }
        return dst;
    }
}
