// Interpreter round i1 wave 4, lane L1 — the two superinstructions this wave
// added to the raw-bytecode fast path, plus their throwing edge:
//
//   arrayLoop   aload a; arraylength            (javac's `i < a.length`)
//   sumLocals   iload_1; iload_2; iadd; istore_1 (`s += i` over low locals)
//   nullLength  aload_1; arraylength on null   (must throw NPE from the
//                                               arraylength, never return)
//
// stdout: deterministic lines that must match HotSpot 25 exactly:
//   arrayLoop 1999999000000
//   sumLocals 2839207360
//   nullLength java.lang.NullPointerException
// (with the default n = 2_000_000; the sums are n*(n-1)/2, the second one
// wrapped to 32 bits by `int` arithmetic and printed unsigned).
// stderr: ns per loop iteration per arm, min over the rounds, arms run in
// alternating order.
//
// Measure: CratonVM `--nojit` built before and after lane L1 wave 4, runs
// interleaved (A B A B ...), compare medians of the stderr numbers; HotSpot
// `-Xint` is the reference column. Arguments: [n] [rounds].
public class L1Wave4Fusions {
    // a in local 0, s in 1 (a long, so 1-2), i in 3.
    static long arrayLoop(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    // n in local 0, s in 1, i in 2: `s += i` is iload_1; iload_2; iadd; istore_1.
    static int sumLocals(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static int lengthOf(int[] a) {
        int[] b = a;
        return b.length;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
        }
        String[] names = {"arrayLoop", "sumLocals"};
        long[] best = {Long.MAX_VALUE, Long.MAX_VALUE};
        long[] sums = new long[2];
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < 2; k++) {
                int arm = (r % 2 == 0) ? k : 1 - k;
                long t0 = System.nanoTime();
                // The int sum wraps; widen it the same way on every VM.
                long v = arm == 0 ? arrayLoop(a) : (long) sumLocals(n) & 0xffffffffL;
                long dt = System.nanoTime() - t0;
                if (dt < best[arm]) {
                    best[arm] = dt;
                }
                sums[arm] = v;
            }
        }
        System.out.println(names[0] + " " + sums[0]);
        System.out.println(names[1] + " " + sums[1]);
        try {
            System.out.println("nullLength returned " + lengthOf(null));
        } catch (NullPointerException e) {
            System.out.println("nullLength " + e.getClass().getName());
        }
        for (int arm = 0; arm < 2; arm++) {
            System.err.printf("%-10s %.2f ns/iter%n", names[arm], (double) best[arm] / n);
        }
    }
}
