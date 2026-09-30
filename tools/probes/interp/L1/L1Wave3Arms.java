// Interpreter round i1, wave 3, lane L1 — the opcodes that gained a
// raw-bytecode arm this wave (newarray, anewarray, athrow, invokedynamic) and
// the reference-local spellings that now share one implementation
// (aload_<n> / aload n, astore_<n> / astore n).
//
// Run with the interpreter only (CratonVM `--nojit`, HotSpot `-Xint`) and diff
// stdout line by line against HotSpot 25: every stdout line is deterministic
// and must match exactly. Timings go to stderr (median of 5): interleave with
// a `--noverify` run of the same binary to price the fast arms against the
// decoded path they replaced; in-JVM timings swing up to ~3x between reps on
// the development hosts, so compare medians.
//
//   arrays   newarray of every primitive type, anewarray of a class and of an
//            array type, zero length, NegativeArraySizeException, the
//            component class names
//   throw    athrow caught in the same frame, one frame up, rethrown from a
//            finally, and athrow of null (class name only: the helpful-NPE
//            text depends on -g)
//   indy     string concatenation and lambda capture (invokedynamic)
//   locals   a method with more than four reference locals: the explicit-index
//            aload / astore forms, null in a high local, identity through both
//            spellings
public class L1Wave3Arms {
    static int MINUS_ONE = -1;

    static void arrays() {
        boolean[] z = new boolean[3];
        char[] c = new char[4];
        float[] f = new float[5];
        double[] d = new double[6];
        byte[] b = new byte[7];
        short[] s = new short[8];
        int[] i = new int[9];
        long[] l = new long[10];
        System.out.println("arrays prim " + z.length + " " + c.length + " " + f.length + " "
                + d.length + " " + b.length + " " + s.length + " " + i.length + " " + l.length);
        System.out.println("arrays names " + z.getClass().getName() + " " + l.getClass().getName());
        String[] strs = new String[2];
        int[][] rows = new int[3][];
        Object[] empty = new Object[0];
        System.out.println("arrays ref " + strs.length + " " + strs.getClass().getName() + " "
                + rows.length + " " + rows.getClass().getName() + " " + empty.length
                + " " + (strs[0] == null) + " " + (rows[1] == null));
        try {
            int[] bad = new int[MINUS_ONE];
            System.out.println("arrays neg-int no throw " + bad.length);
        } catch (NegativeArraySizeException e) {
            System.out.println("arrays neg-int " + e.getMessage());
        }
        try {
            String[] bad = new String[MINUS_ONE * 3];
            System.out.println("arrays neg-ref no throw " + bad.length);
        } catch (NegativeArraySizeException e) {
            System.out.println("arrays neg-ref " + e.getMessage());
        }
    }

    static int thrower(int k) {
        if (k % 3 == 0) {
            throw new IllegalStateException("k" + k);
        }
        return k;
    }

    static int finallyRethrow(int k) {
        try {
            return thrower(k);
        } finally {
            k++;
        }
    }

    static void throwing() {
        int caught = 0, sum = 0;
        for (int k = 0; k < 30; k++) {
            try {
                sum += finallyRethrow(k);
            } catch (IllegalStateException e) {
                caught++;
                if (k == 9) {
                    System.out.println("throw msg " + e.getMessage());
                }
            }
        }
        System.out.println("throw caught " + caught + " sum " + sum);
        try {
            RuntimeException r = null;
            throw r;
        } catch (NullPointerException e) {
            System.out.println("throw null -> " + e.getClass().getName());
        }
        try {
            try {
                throw new UnsupportedOperationException("inner");
            } catch (IllegalStateException wrong) {
                System.out.println("throw wrong handler");
            }
        } catch (UnsupportedOperationException e) {
            System.out.println("throw outer " + e.getMessage());
        }
    }

    interface IntOp {
        int apply(int x);
    }

    static void indy() {
        int total = 0;
        for (int k = 0; k < 50; k++) {
            String s = "v" + k + ":" + (k * 3L) + '/' + (k % 2 == 0);
            total += s.length();
        }
        System.out.println("indy concat " + total + " " + ("a" + 1 + 'b' + 2.5 + null));
        int base = 7;
        IntOp add = x -> x + base;
        IntOp twice = x -> add.apply(add.apply(x));
        System.out.println("indy lambda " + twice.apply(1) + " " + add.apply(-7));
    }

    static String locals(Object a, Object b, Object c, Object d) {
        Object e = a;          // aload_0; astore 4
        Object f = null;       // aconst_null; astore 5
        Object g = b;          // aload_1; astore 6
        Object h = e;          // aload 4; astore 7
        if (f != null) {
            return "locals f not null";
        }
        return "locals " + (h == a) + " " + (g == b) + " " + (e == h) + " " + (f == null)
                + " " + c + " " + d;
    }

    static long burnNewarray(int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            int[] a = new int[4];
            s += a.length;
        }
        return s;
    }

    static long burnThrow(int n) {
        long s = 0;
        RuntimeException e = new RuntimeException("x");
        for (int k = 0; k < n; k++) {
            try {
                throw e;
            } catch (RuntimeException r) {
                s++;
            }
        }
        return s;
    }

    static long burnConcat(int n) {
        long s = 0;
        for (int k = 0; k < n; k++) {
            s += ("" + k).length();
        }
        return s;
    }

    static void time(String name, java.util.function.LongSupplier body) {
        long[] t = new long[5];
        long r = 0;
        for (int rep = 0; rep < t.length; rep++) {
            long t0 = System.nanoTime();
            r = body.getAsLong();
            t[rep] = System.nanoTime() - t0;
        }
        java.util.Arrays.sort(t);
        System.err.println("[L1Wave3Arms] " + name + " median_ms=" + (t[2] / 1_000_000)
                + " result=" + r);
    }

    public static void main(String[] args) {
        arrays();
        throwing();
        indy();
        System.out.println(locals("A", "B", "C", "D"));
        time("newarray", () -> burnNewarray(2_000_000));
        time("athrow", () -> burnThrow(200_000));
        time("indy-concat", () -> burnConcat(200_000));
        System.out.println("done");
    }
}
