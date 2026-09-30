// Interpreter round i1 wave 2, lane L1 — prices the javac-shaped
// superinstructions this wave added to the raw-bytecode fast path:
//
//   lowLocals    iload_2; iload_0; if_icmpge   (top-tested loop test)
//                iinc 2 1; goto                (loop step)
//   highLocals   iload 5; iload_3; if_icmpge   (counter past local 3)
//                iload 5; bipush 7; if_icmpeq
//   constBounds  iload; sipush 1000; if_icmpge, iload; iconst_2; if_icmple
//   fields       aload_0; getfield             (twice per iteration)
//
// stdout: one deterministic checksum line per arm (must match HotSpot 25).
// stderr: ns per loop iteration per arm, min over the rounds, arms run in
// alternating order so host drift hits every arm equally.
//
// Measure: CratonVM `--nojit` built before and after lane L1 wave 2, runs
// interleaved (A B A B ...), compare medians of the stderr numbers; HotSpot
// `-Xint` is the reference column. Arguments: [n] [rounds].
public class L1FusionBurn {
    int f0 = 3;
    int f1 = 5;

    // n in local 0, s in 1, i in 2.
    static int lowLocals(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += i & 7;
        }
        return s;
    }

    // a, b, c, n in locals 0..3, s in 4, i in 5.
    static int highLocals(int a, int b, int c, int n) {
        int s = a + b + c;
        for (int i = 0; i < n; i++) {
            if (i != 7) {
                s ^= i;
            }
        }
        return s;
    }

    static int constBounds(int reps) {
        int s = 0;
        for (int r = 0; r < reps; r++) {
            for (int i = 0; i < 1000; i++) {
                if (i > 2) {
                    s++;
                }
            }
        }
        return s;
    }

    int fields(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += f0 + f1;
        }
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        L1FusionBurn o = new L1FusionBurn();
        String[] names = {"lowLocals", "highLocals", "constBounds", "fields"};
        long[] best = {Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE};
        long[] sums = new long[4];
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < 4; k++) {
                int arm = (r % 2 == 0) ? k : 3 - k;
                long t0 = System.nanoTime();
                long v;
                switch (arm) {
                    case 0:
                        v = lowLocals(n);
                        break;
                    case 1:
                        v = highLocals(1, 2, 3, n);
                        break;
                    case 2:
                        v = constBounds(n / 1000);
                        break;
                    default:
                        v = o.fields(n);
                        break;
                }
                long dt = System.nanoTime() - t0;
                if (dt < best[arm]) {
                    best[arm] = dt;
                }
                sums[arm] = v;
            }
        }
        for (int arm = 0; arm < 4; arm++) {
            System.out.println(names[arm] + " " + sums[arm]);
        }
        for (int arm = 0; arm < 4; arm++) {
            System.err.printf("%-12s %.2f ns/iter%n", names[arm], (double) best[arm] / n);
        }
    }
}
