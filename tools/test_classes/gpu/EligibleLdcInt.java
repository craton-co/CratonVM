public class EligibleLdcInt {
    // AUDIT C31 follow-up (2026-07-11): int literals outside the sipush
    // range (-32768..32767) cannot use bipush/sipush and compile to
    // `ldc`/`ldc_w` (0x12/0x13) instead. Both 1_000_003 and 77_777
    // exceed that range, so before this fix this otherwise-plain
    // element-wise map tripped Reason::LoadConstant and was rejected
    // outright — the single most common real-world eligibility killer.
    public static void scale(int[] a, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] * 1_000_003 + 77_777;
        }
    }
}
