// Behavioural check on range-based bounds-check elimination.
//
// The unit tests assert what the ANALYSIS decides. This asserts what the
// EMITTED CODE does -- that the loops whose checks were removed still compute
// the right answers, and that every shape the pass must REFUSE still throws
// ArrayIndexOutOfBoundsException. A pass that wrongly elides shows up here as
// a missing exception or a corrupted sum, not as a compile error.
public class BceProbe {
    static int fails = 0;

    static void check(String what, long got, long want) {
        if (got != want) {
            System.out.println("FAIL " + what + ": got " + got + " want " + want);
            fails++;
        }
    }

    static void expectAioobe(String what, Runnable r) {
        try {
            r.run();
            System.out.println("FAIL " + what + ": no AIOOBE");
            fails++;
        } catch (ArrayIndexOutOfBoundsException e) {
            // as specified
        }
    }

    // The exact shape the pass proves: unit stride from 0, bounded by the same
    // array's length.
    static long sum(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) s += a[i];
        return s;
    }

    // Same shape, writing.
    static void fill(int[] a, int v) {
        for (int i = 0; i < a.length; i++) a[i] = v + i;
    }

    // Two arrays, one length. The pass must NOT use a.length to justify b[i]
    // -- and when b is shorter this throws, which is the observable proof.
    static long sumTwo(int[] a, int[] b) {
        long s = 0;
        for (int i = 0; i < a.length; i++) s += a[i] + b[i];
        return s;
    }

    // A stride the proof cannot close. Correctness must be unaffected.
    static long sumEveryOther(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i += 2) s += a[i];
        return s;
    }

    // Off-by-one: `<=` really does run past the end.
    static long sumInclusive(int[] a) {
        long s = 0;
        for (int i = 0; i <= a.length; i++) s += a[i];
        return s;
    }

    // A start that is not provably non-negative, and IS negative.
    static long sumFrom(int[] a, int from) {
        long s = 0;
        for (int i = from; i < a.length; i++) s += a[i];
        return s;
    }

    // Nested: the inner loop is bounded by the inner array.
    static long sumJagged(int[][] g) {
        long s = 0;
        for (int i = 0; i < g.length; i++) {
            int[] row = g[i];
            for (int j = 0; j < row.length; j++) s += row[j];
        }
        return s;
    }

    public static void main(String[] args) {
        int n = 512;
        int[] a = new int[n];
        for (int i = 0; i < n; i++) a[i] = i;
        long want = (long) n * (n - 1) / 2;

        // Warm past the compile thresholds, then assert on compiled bodies.
        long s = 0;
        for (int r = 0; r < 20000; r++) s += sum(a);
        check("sum", s, want * 20000);

        int[] b = new int[n];
        for (int r = 0; r < 20000; r++) fill(b, 0);
        check("fill", sum(b), want);

        long t = 0;
        for (int r = 0; r < 20000; r++) t += sumTwo(a, b);
        check("sumTwo", t, 2L * want * 20000);

        long u = 0;
        for (int r = 0; r < 20000; r++) u += sumEveryOther(a);
        long wantEveryOther = 0;
        for (int i = 0; i < n; i += 2) wantEveryOther += a[i];
        check("sumEveryOther", u, wantEveryOther * 20000);

        long v = 0;
        for (int r = 0; r < 20000; r++) v += sumJagged(new int[][] {a, b});
        check("sumJagged", v, 2L * want * 20000);

        // Every refusal shape must still trap -- and the method has to be HOT
        // first, or this tests the interpreter and proves nothing about the
        // pass. `sumInclusive` throws on every call by construction, so it is
        // warmed inside a catch; `sumFrom` is warmed on a valid start.
        int trapped = 0;
        for (int r = 0; r < 20000; r++) {
            try {
                sumInclusive(a);
            } catch (ArrayIndexOutOfBoundsException e) {
                trapped++;
            }
        }
        check("sumInclusive traps every time", trapped, 20000);

        long w = 0;
        for (int r = 0; r < 20000; r++) w += sumFrom(a, 0);
        check("sumFrom(0)", w, want * 20000);

        expectAioobe("sumInclusive", () -> sumInclusive(a));
        expectAioobe("sumFrom(-1)", () -> sumFrom(a, -1));
        expectAioobe("sumTwo shorter b", () -> sumTwo(a, new int[n - 1]));
        expectAioobe("sum on empty then read", () -> {
            int[] e = new int[0];
            System.out.println(e[0]);
        });

        System.out.println(fails == 0 ? "BCE PROBE OK" : "BCE PROBE FAILED " + fails);
    }
}
