public class EligibleLdcDouble {
    // AUDIT C31 follow-up (2026-07-11): the JVM has no `dconst` beyond
    // `dconst_0`/`dconst_1`, so any other double literal compiles to
    // `ldc2_w` (0x14) — Euler's number here, chosen for a mantissa that
    // does not round-trip through a short decimal, so a wrong bit or a
    // stray single-precision narrowing in the lowering would be caught
    // by an exact-bit PTX comparison.
    public static void fma(double[] a, double[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] * 2.718281828459045;
        }
    }
}
