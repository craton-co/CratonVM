// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The `charAt` + unrelated-field shape of `NOTES-opts7.md` section 3 with the
 * String replaced by an `int[]`, so it reaches the optimizing tier: a bounds-
 * guarded array read and a loop-invariant read of `g.bias` under an `if`.
 *
 * It is the NEGATIVE control for `GuardedDivHoist`: all four arms agree
 * (`hard_barrier=true writes_memory=false`, only the `arraylength` hoists), and
 * the guard machinery is not why. Once LICM moves `a.length` into the
 * pre-header, `preheader_may_trap` is true and the read-hoist arm is not
 * entered; and `g` is a parameter under a conditional, so `hoist_is_safe`
 * would refuse it regardless -- HotSpot hoists it by speculating non-null,
 * which this IR does not do. Expected checksum: r=3847000.
 *
 *   CRATONVM_DBG_LICM=1 cratonvm -cp probes GuardedFieldArr
 */
public class GuardedFieldArr {
    int bias = 1;
    static int[] s;

    static int run(GuardedFieldArr g, int reps) {
        int c = 0;
        int[] a = s;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < a.length; i++)
                if (a[i] == 0) c += g.bias;
        return c;
    }

    public static void main(String[] args) {
        s = new int[100_000];
        for (int i = 0; i < s.length; i++) s[i] = i % 26;
        GuardedFieldArr g = new GuardedFieldArr();
        for (int w = 0; w < 30000; w++) run(g, 1);
        long t0 = System.nanoTime();
        int r = run(g, 1000);
        long t1 = System.nanoTime();
        System.out.printf("steady %.3f ns/elem r=%d%n", (t1 - t0) / (1000.0 * s.length), r);
    }
}
