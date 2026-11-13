/**
 * j27 wave 2 (lane SOUNDREV): adversarial rows for the `a[p - c]` floor arm
 * (CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET, opt-in; ir_check_elim
 * `monotone_phi_at_least`, planter ir_optimize/monotone_predicate/neg_offset.rs)
 * that J27NegOffsetRows does not cover:
 *
 *  - an inner loop whose start is the OUTER loop's variable, so the floor
 *    guard `1 <= j` sits in the inner pre-header inside the outer loop and
 *    fails on a later outer trip (deopt mid-method, completed inner loops'
 *    side effects already stored);
 *  - a decode-style index that advances by 1 or 2 on data (the reader must
 *    fail closed: no room test bounds the +2 step).
 *
 * Every row is int/long arithmetic over a[k] = k, length 64, so the expected
 * lines are HotSpot's on any JVM.
 *
 * Expected HotSpot 25 output:
 *   fact nested-1-8 15568
 *   fact nested-throw java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 64
 *   fact nested-throw-partial 5858
 *   fact nested-rewarm 15568
 *   fact multistep-1 961
 *   fact multistep-3 962
 *   fact multistep-0 java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 64
 *   fact multistep-rewarm 961
 *
 * java J27SoundrevNegOffsetRows
 * (CratonVM: CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET=1 cratonvm J27SoundrevNegOffsetRows,
 *  and the same with =0; both must print the lines above.)
 */
public class J27SoundrevNegOffsetRows {
    static long partial;

    /** for j = hi down to lo: for (i = j; i < a.length; i++) s += a[i - 1]. */
    static long nested(int[] a, int lo, int hi) {
        long s = 0;
        partial = 0;
        for (int j = hi; j >= lo; j--) {
            for (int i = j; i < a.length; i++) {
                s += a[i - 1];
            }
            partial = s;
        }
        return s;
    }

    /** p from `from`; s += a[p - 1]; p += 2 on an even a[p], else 1. */
    static long multistep(int[] a, int from) {
        long s = 0;
        int p = from;
        while (p < a.length) {
            s += a[p - 1];
            if ((a[p] & 1) == 0) {
                p += 2;
            } else {
                p += 1;
            }
        }
        return s;
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    public static void main(String[] args) {
        int[] a = new int[64];
        for (int k = 0; k < a.length; k++) a[k] = k;

        long w = 0;
        for (int r = 0; r < 20000; r++) {
            w += nested(a, 1, 8);
            w += multistep(a, 1 + (r & 2));
        }
        if (w != 20000L * 15568 + 10000L * 961 + 10000L * 962) {
            fact("warm-mismatch", w);
        }

        fact("nested-1-8", nested(a, 1, 8));
        try {
            fact("nested-throw-none", nested(a, 0, 3));
        } catch (ArrayIndexOutOfBoundsException e) {
            fact("nested-throw", e);
        }
        fact("nested-throw-partial", partial);
        long rw = 0;
        for (int r = 0; r < 20000; r++) rw = nested(a, 1, 8);
        fact("nested-rewarm", rw);

        fact("multistep-1", multistep(a, 1));
        fact("multistep-3", multistep(a, 3));
        try {
            fact("multistep-0-none", multistep(a, 0));
        } catch (ArrayIndexOutOfBoundsException e) {
            fact("multistep-0", e);
        }
        long mw = 0;
        for (int r = 0; r < 20000; r++) mw = multistep(a, 1);
        fact("multistep-rewarm", mw);
    }
}
