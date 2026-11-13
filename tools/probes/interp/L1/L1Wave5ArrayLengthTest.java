// Interpreter round i1 wave 5, lane L1 — javac's array-loop test
// `iload i; aload a; arraylength; if_icmp<cond>` now runs as ONE fused
// compare-and-branch (`fused_icmp_arm!` with an `aload; arraylength` second
// operand), in the short (`aload_0`) and wide (`iload 7`, `aload 4`) spellings,
// and the `aload n; arraylength` pair fuses too. A null array must still throw
// the NPE from the `arraylength`, never branch or return.
//
// stdout: deterministic lines that must match HotSpot 25 exactly:
//   low 1999999000000
//   high 1999999000000
//   down 2000000
//   ne 1999999000000
//   nullLow java.lang.NullPointerException
//   nullHigh java.lang.NullPointerException
//   pairHigh 2000000
// (with the default n = 2_000_000).
// stderr: ns per loop iteration for `low` and `high`, min over the rounds.
//
// Measure: CratonVM `--compatible --nojit`, wave-4 binary against wave-5,
// runs interleaved (A B A B ...), medians of the stderr numbers; with
// `tools/probes/interp/L1/L1Wave4Fusions.java`'s `arrayLoop` as the second
// witness (it is `low`). HotSpot `-Xint` is the reference column.
// Arguments: [n] [rounds].
public class L1Wave5ArrayLengthTest {
    // a in local 0, s in 1-2, i in 3: `iload_3; aload_0; arraylength; if_icmpge`.
    static long low(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    // x in 0-1, y in 2-3, a in 4, s in 5-6, i in 7:
    // `iload 7; aload 4; arraylength; if_icmpge`.
    static long high(long x, long y, int[] a) {
        long s = x + y;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    // a in 0, c in 1, i in 2 (`i != a.length`):
    // `iload_2; aload_0; arraylength; if_icmpeq`.
    static int down(int[] a) {
        int c = 0;
        for (int i = 0; i != a.length; i++) {
            c++;
        }
        return c;
    }

    // `i <= a.length - 1` is not the fused shape; `a.length > i` puts the
    // length FIRST (`aload; arraylength; iload; if_icmple`), also not fused —
    // both must keep their answers.
    static long ne(int[] a) {
        long s = 0;
        for (int i = 0; a.length > i; i++) {
            s += a[i];
        }
        return s;
    }

    // x in 0-1, y in 2-3, a in 4: `aload 4; arraylength` (the pair).
    static int pairHigh(long x, long y, int[] a) {
        return a.length + (int) (x - y);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
        }
        long bestLow = Long.MAX_VALUE;
        long bestHigh = Long.MAX_VALUE;
        long sLow = 0;
        long sHigh = 0;
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < 2; k++) {
                boolean lowFirst = (r % 2 == 0) == (k == 0);
                long t0 = System.nanoTime();
                if (lowFirst) {
                    sLow = low(a);
                } else {
                    sHigh = high(0L, 0L, a);
                }
                long dt = System.nanoTime() - t0;
                if (lowFirst) {
                    bestLow = Math.min(bestLow, dt);
                } else {
                    bestHigh = Math.min(bestHigh, dt);
                }
            }
        }
        System.out.println("low " + sLow);
        System.out.println("high " + sHigh);
        System.out.println("down " + down(a));
        System.out.println("ne " + ne(a));
        try {
            System.out.println("nullLow returned " + low(null));
        } catch (NullPointerException e) {
            System.out.println("nullLow " + e.getClass().getName());
        }
        try {
            System.out.println("nullHigh returned " + high(1L, 2L, null));
        } catch (NullPointerException e) {
            System.out.println("nullHigh " + e.getClass().getName());
        }
        System.out.println("pairHigh " + pairHigh(5L, 5L, a));
        System.err.printf("low  %.2f ns/iter%n", (double) bestLow / n);
        System.err.printf("high %.2f ns/iter%n", (double) bestHigh / n);
    }
}
