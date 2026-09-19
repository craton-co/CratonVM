public class EligibleLdcFloat {
    // AUDIT C31 follow-up (2026-07-11): the JVM has no `fconst` beyond
    // `fconst_0`/`fconst_1`/`fconst_2`, so any other float literal
    // compiles to `ldc`/`ldc_w` (0x12/0x13). Two such literals here
    // (3.14159265f, 1.5f) exercise both the constant-pool resolution
    // and the exact-bit PTX hex-literal lowering for more than one
    // Float CP entry in the same method.
    public static void fma(float[] a, float[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] * 3.14159265f + 1.5f;
        }
    }
}
