package gpu.annotations;

import craton.gpu.GpuKernel;
import craton.gpu.AdmissionHint;
import craton.gpu.GpuExclude;

// Does `offload_jit_gate` ask the same question as the dispatcher about
// ANNOTATIONS? Two kernels, both invisible to a strict analyzer, for
// opposite reasons.
public class GateAnnotationParity {

    // Eligible ONLY with the hint: `Math.sqrt` is an `invokestatic`, so
    // the strict verdict is Rejected(Invoke). A gate that judges without
    // annotations never registers this with the offload hook, so once
    // `drive` compiles its call site binds directly and goes dark.
    @GpuKernel(admit = AdmissionHint.ALLOW_INTRINSIC_CALLS)
    public static void hinted(double[] src, double[] dst) {
        int n = src.length;
        for (int i = 0; i < n; i++) {
            dst[i] = Math.sqrt(src[i]) * 2.0 + 1.0;
        }
    }

    // The other direction. Structurally a fine kernel, so a gate that
    // judges without annotations calls it Eligible and REGISTERS it --
    // arming the hook for a method the dispatcher short-circuits on
    // @GpuExclude and will never launch.
    @GpuExclude(reason = "gate/dispatcher parity probe")
    public static void excluded(double[] src, double[] dst) {
        int n = src.length;
        for (int i = 0; i < n; i++) {
            dst[i] = src[i] * 3.0 - 7.0;
        }
    }

    static void drive(String which, int iters, double[] a, double[] b) {
        for (int k = 0; k < iters; k++) {
            if (which.equals("hinted")) hinted(a, b);
            else excluded(a, b);
        }
    }

    public static void main(String[] args) {
        String which = args[0];
        int n = Integer.parseInt(args[1]);
        int iters = Integer.parseInt(args[2]);
        double[] a = new double[n], b = new double[n];
        for (int i = 0; i < n; i++) a[i] = i + 1;
        drive(which, iters, a, b);
        double s = 0;
        for (int i = 0; i < n; i += 4096) s += b[i];
        System.out.println("which=" + which + " checksum=" + s);
    }
}
