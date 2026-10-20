/**
 * The shape the register-to-register phi copy could get wrong: an edge whose
 * parallel copy is a CYCLE.
 *
 * `swap2` makes each loop-carried local the other's back-edge value, so the
 * edge's copy list is `a <- b, b <- a` and `resolve_parallel_copy` has to break
 * it through the scratch word. `rot3` is the same with a three-cycle, which
 * needs the save/restore pair to unwind rather than just to reorder.
 *
 * Emitted in gather order the cycle reads a word the previous copy already
 * overwrote and both locals end up holding one value — a wrong ANSWER, not a
 * slow one, which is why this probe prints checksums rather than a time.
 */
public class PhiSwapLoop {
    static long swap2(int n) {
        int a = 1, b = 2;
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int ta = a, tb = b;
            a = tb;
            b = ta + 1;
            acc += a * 31L + b;
        }
        return acc * 1000003L + a * 7L + b;
    }

    static long rot3(int n) {
        int a = 1, b = 2, c = 3;
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int ta = a, tb = b, tc = c;
            a = tc;
            b = ta;
            c = tb + 1;
            acc += a * 17L + b * 5L + c;
        }
        return acc * 1000003L + a * 7L + b * 11L + c;
    }

    /** Loop-carried values of both banks at once, so FP and GP phis mix. */
    static long mixed(int n) {
        int i2 = 0;
        long l = 1;
        double d = 0.5;
        for (int i = 0; i < n; i++) {
            int t = i2;
            i2 = (int) (l & 0xFFFF);
            l = l * 3 + t;
            d = d * 1.0000001 + 1.0;
        }
        return i2 * 1000003L + l + (long) d;
    }

    public static void main(String[] args) {
        int n = Integer.getInteger("probe.n", 100000);
        int reps = Integer.getInteger("probe.reps", 3000);
        long s = 0, r = 0, m = 0;
        for (int rep = 0; rep < reps; rep++) {
            s = swap2(n);
            r = rot3(n);
            m = mixed(n);
        }
        System.out.println("swap2=" + s);
        System.out.println("rot3=" + r);
        System.out.println("mixed=" + m);
    }
}
