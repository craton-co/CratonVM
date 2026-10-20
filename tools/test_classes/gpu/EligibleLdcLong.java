public class EligibleLdcLong {
    // AUDIT C31 follow-up (2026-07-11): the JVM has no `lconst_2` (only
    // `lconst_0`/`lconst_1`), so ANY long literal other than 0L/1L
    // compiles to `ldc2_w` (0x14). 6364136223846793005L is the
    // splitmix64 / MMIX LCG multiplier — chosen because it is far
    // outside int range, so it is unambiguously a wide constant-pool
    // load rather than e.g. an `i2l` of a small pushed int.
    public static void mix(long[] a, long[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] * 6364136223846793005L;
        }
    }
}
