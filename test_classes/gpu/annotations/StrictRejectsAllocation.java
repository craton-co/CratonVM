package gpu.annotations;
import craton.gpu.GpuKernel;
import craton.gpu.AdmissionHint;
public class StrictRejectsAllocation {
    @GpuKernel(admit = AdmissionHint.STRICT)
    public static int[] mapSquare(int[] src) {
        int n = src.length;
        int[] dst = new int[n];
        for (int i = 0; i < n; i++) {
            dst[i] = src[i] * src[i];
        }
        return dst;
    }
}
