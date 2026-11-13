// j27 wave 2 (lane TIER1PROP): `a[i - c]` under a loop index with a
// NON-constant start (BCE21-2's unbuilt half; switch
// CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET=1, opt-in). The monotone predicate
// plants `to <= a.length` and `c <= from` in the pre-header; a failing guard
// deopts to the loop entry and the interpreter raises the exception itself.
// Book: docs/feature-designs/jit-ir-optimizations.md (BCE21-2).
//
// Every row is a computed value or an exception class + message; none depends
// on timing or on which tier ran. Run each arm, both modes, and diff against
// HotSpot:
//   java -cp <dir> J27NegOffsetRows                                  > hs.txt
//   cratonvm --cp <dir> J27NegOffsetRows                              | diff - hs.txt
//   CRATONVM_JIT_IR_MONOTONE_NEG_OFFSET=1 cratonvm --cp <dir> J27NegOffsetRows | diff - hs.txt
//   (and the same two with --compatible, and once with --nojit)
// Engagement (switch on): CRATONVM_DBG_LICM=1 prints
//   `[ir-licm] monotone predicate: header .. planted 2` for shiftSum / copyBack
//   and `planted 1` for ownLen.
//
// Expected HotSpot 25 output (facts only):
//   fact shift-1-9 148
//   fact shift-3-6 32
//   fact warm-shift 29000000
//   fact copy-checksum 1108
//   fact warm-own 12100000
//   fact own-2 61
//   fact own-5 53
//   fact shift-low java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 8
//   fact shift-low-zero-trip 0
//   fact shift-high java.lang.ArrayIndexOutOfBoundsException: Index 8 out of bounds for length 8
//   fact shift-null java.lang.NullPointerException
//   fact shift-null-zero-trip 0
//   fact shift-wrap java.lang.ArrayIndexOutOfBoundsException: Index 2147483647 out of bounds for length 8
//   fact copy-over java.lang.ArrayIndexOutOfBoundsException: Index 8 out of bounds for length 8
//   fact copy-over-partial 1108
//   fact own-low java.lang.ArrayIndexOutOfBoundsException: Index -1 out of bounds for length 8
//   fact own-zero-trip 0
//   fact own-empty 0
//   fact own-null java.lang.NullPointerException
//   fact rewarm-shift 29000000
//   fact rewarm-own 12100000
// -Dj27.timing=true adds `time shift-ns-per-call <n>` (not compared).
import java.util.function.LongSupplier;

public class J27NegOffsetRows {
    static final int N = 200_000;

    /** `for (i = from; i < to; i++) s += a[i - 1]`: both guards. */
    static long shiftSum(int[] a, int from, int to) {
        long s = 0;
        for (int i = from; i < to; i++) {
            s += a[i - 1];
        }
        return s;
    }

    /** `d[i] = s[i - 1]`: two arrays under one bound. */
    static void copyBack(int[] d, int[] s, int from, int to) {
        for (int i = from; i < to; i++) {
            d[i] = s[i - 1];
        }
    }

    /** The array's own length bounds the loop: a lone `2 <= from` floor. */
    static long ownLen(int[] a, int from) {
        long s = 0;
        for (int i = from; i < a.length; i++) {
            s += a[i - 2];
        }
        return s;
    }

    static int[] squares() {
        int[] a = new int[8];
        for (int k = 0; k < a.length; k++) {
            a[k] = k * k + 1; // 1 2 5 10 17 26 37 50
        }
        return a;
    }

    static long checksum(int[] d) {
        long c = 0;
        for (int i = 0; i < d.length; i++) {
            c += (long) d[i] * (i + 1);
        }
        return c;
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    /** The row's value, or its exception's class and message. */
    static String outcome(LongSupplier row) {
        try {
            return Long.toString(row.getAsLong());
        } catch (ArrayIndexOutOfBoundsException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        } catch (NullPointerException e) {
            // Class only: the helpful-NPE text names locals by debug info.
            return e.getClass().getName();
        }
    }

    static long warmShift(int[] a) {
        long t = 0;
        for (int k = 0; k < N; k++) {
            t += shiftSum(a, 1 + (k & 3), 9);
        }
        return t;
    }

    static long warmOwn(int[] a) {
        long t = 0;
        for (int k = 0; k < N; k++) {
            t += ownLen(a, 2 + (k & 1));
        }
        return t;
    }

    public static void main(String[] args) {
        int[] a = squares();
        fact("shift-1-9", shiftSum(a, 1, 9));
        fact("shift-3-6", shiftSum(a, 3, 6));
        fact("warm-shift", warmShift(a));
        int[] d = new int[9];
        for (int k = 0; k < N / 8; k++) {
            copyBack(d, a, 1, 9);
        }
        fact("copy-checksum", checksum(d));
        fact("warm-own", warmOwn(a));
        fact("own-2", ownLen(a, 2));
        fact("own-5", ownLen(a, 5));

        // Deopt and exception edges, with the bodies compiled above.
        fact("shift-low", outcome(() -> shiftSum(a, 0, 9)));
        fact("shift-low-zero-trip", outcome(() -> shiftSum(a, 0, 0)));
        fact("shift-high", outcome(() -> shiftSum(a, 1, 10)));
        fact("shift-null", outcome(() -> shiftSum(null, 1, 2)));
        fact("shift-null-zero-trip", outcome(() -> shiftSum(null, 1, 1)));
        fact("shift-wrap",
                outcome(() -> shiftSum(a, Integer.MIN_VALUE, Integer.MIN_VALUE + 2)));
        int[] d2 = new int[9];
        fact("copy-over", outcome(() -> {
            copyBack(d2, a, 1, 10);
            return 0;
        }));
        fact("copy-over-partial", checksum(d2));
        fact("own-low", outcome(() -> ownLen(a, 1)));
        fact("own-zero-trip", outcome(() -> ownLen(a, 9)));
        fact("own-empty", outcome(() -> ownLen(new int[0], 0)));
        fact("own-null", outcome(() -> ownLen(null, 2)));

        // After the guards failed (and possibly de-specialised the header),
        // the same answers again.
        fact("rewarm-shift", warmShift(a));
        fact("rewarm-own", warmOwn(a));

        if (Boolean.getBoolean("j27.timing")) {
            long sink = 0;
            long t0 = System.nanoTime();
            for (int k = 0; k < N; k++) {
                sink += shiftSum(a, 1, 9);
            }
            long t1 = System.nanoTime();
            System.out.println("time shift-ns-per-call " + (t1 - t0) / N + " sink=" + sink);
        }
    }
}
