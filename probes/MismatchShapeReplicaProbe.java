/**
 * Is the ArraysSupport.mismatch miscompile in the METHOD'S OWN compiled
 * bytecode, or in the compiled call into the vectorizedMismatch native?
 *
 * Both possibilities produce the same visible answer (-1 for every mismatch
 * position except 0), so they cannot be told apart from Arrays.mismatch alone:
 *
 *   - if the native is handed bad arguments it returns -1, and the caller's
 *     `i = length - ~i` becomes `length`, skipping the scalar tail loop;
 *   - if the caller's `if (i >= 0) return i` is compiled wrongly, a CORRECT
 *     native answer k falls through to `i = length - ~k` = length + k + 1,
 *     which also skips the loop.
 *
 * This probe rebuilds the exact method shape in pure Java, with a plain Java
 * callee in place of the native. If it reproduces, the defect is in the
 * compiled bytecode shape and has nothing to do with native marshaling.
 */
public class MismatchShapeReplicaProbe {

    /** Stands in for jdk.internal.util.ArraysSupport.vectorizedMismatch. */
    static int fakeVectorized(Object a, long aOffset, Object b, long bOffset,
                              int length, int log2ArrayIndexScale) {
        int[] ia = (int[]) a;
        int[] ib = (int[]) b;
        for (int i = 0; i < length; i++) {
            if (ia[i] != ib[i]) {
                return i;
            }
        }
        return -1;
    }

    /** The exact shape of ArraysSupport.mismatch(int[], int[], int). */
    static int mismatch(int[] a, int[] b, int length) {
        int i = 0;
        if (length > 1) {
            if (a[0] != b[0]) {
                return 0;
            }
            i = fakeVectorized(a, 16L, b, 16L, length, 2);
            if (i >= 0) {
                return i;
            }
            i = length - ~i;
        }
        for (; i < length; i++) {
            if (a[i] != b[i]) {
                return i;
            }
        }
        return -1;
    }

    public static void main(String[] args) {
        final int reps = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
        long bad = 0;
        for (int rep = 0; rep < reps; rep++) {
            int len = 2 + (rep % 20);
            int flip = 1 + (rep % (len - 1));
            int[] a = new int[len];
            int[] b = new int[len];
            for (int i = 0; i < len; i++) {
                a[i] = i + 1;
                b[i] = i + 1;
            }
            b[flip] = -99;
            int got = mismatch(a, b, len);
            if (got != flip) {
                bad++;
                if (bad <= 10) {
                    System.out.println("REPLICA-WRONG rep=" + rep + " len=" + len
                            + " flip=" + flip + " got=" + got);
                }
            }
        }
        System.out.println("MismatchShapeReplicaProbe reps=" + reps + " bad=" + bad);
        if (bad != 0) {
            System.exit(1);
        }
    }
}
