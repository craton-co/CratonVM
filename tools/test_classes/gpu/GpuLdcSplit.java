// Distinguishes two hypotheses for why the compiled-caller hook drops
// some kernels:
//   (a) "64-bit element arrays"  -- the page's hypothesis
//   (b) "kernel body needs a constant-pool constant (ldc/ldc2_w)"
//
// The two disagree on both of these:
//   bigConstI : int[]  kernel, constants too large for sipush -> ldc
//   noLdcJ    : long[] kernel, only lconst_1 -> no ldc at all
public class GpuLdcSplit {
    // int[], small constants: bipush/sipush, no constant pool.
    static void smallConstI(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) { int v = in[i]; out[i] = v * 3 - 7; }
    }
    // int[], constants > 32767: javac must emit ldc.
    static void bigConstI(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) { int v = in[i]; out[i] = v * 1000003 - 999983; }
    }
    // long[], only lconst_1: no ldc2_w anywhere.
    static void noLdcJ(long[] in, long[] out) {
        for (int i = 0; i < out.length; i++) { long v = in[i]; out[i] = v + 1L; }
    }
    // long[], ldc2_w constants.
    static void ldcJ(long[] in, long[] out) {
        for (int i = 0; i < out.length; i++) { long v = in[i]; out[i] = v * 3L - 7L; }
    }

    static void drive(String which, int iters, int[] iIn, int[] iOut, long[] jIn, long[] jOut) {
        for (int k = 0; k < iters; k++) {
            switch (which) {
                case "smallConstI": smallConstI(iIn, iOut); break;
                case "bigConstI":   bigConstI(iIn, iOut);   break;
                case "noLdcJ":      noLdcJ(jIn, jOut);      break;
                case "ldcJ":        ldcJ(jIn, jOut);        break;
            }
        }
    }

    public static void main(String[] args) {
        String which = args[0];
        int n = Integer.parseInt(args[1]);
        int iters = Integer.parseInt(args[2]);
        int[] iIn = new int[n], iOut = new int[n];
        long[] jIn = new long[n], jOut = new long[n];
        for (int i = 0; i < n; i++) { iIn[i] = i; jIn[i] = i; }
        drive(which, iters, iIn, iOut, jIn, jOut);
        long s = 0;
        for (int i = 0; i < n; i += 4096) s += iOut[i] + jOut[i];
        System.out.println("which=" + which + " checksum=" + s);
    }
}
