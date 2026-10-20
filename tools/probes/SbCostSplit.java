/**
 * SbCostSplit — decompose what a StringBuilder native costs, by subtracting
 * rungs that share a prefix of the same work.
 *
 *   identityHashCode : the JIT->native boundary alone, one object argument
 *   sb.length()      : boundary + one StringBuilder field read
 *   sb.append(char)  : boundary + the view + the bulk write + the count write
 *   sb.setLength(0)  : boundary + the count write
 *
 * The builder is pre-sized so no rung ever grows the payload; growth is
 * amortised and would land in whichever rung happened to trip it.
 *
 * Every rung prints a checksum. Kernels live in static methods, not in main.
 */
public class SbCostSplit {

    static long idHash(Object[] objs, int rounds) {
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            acc += System.identityHashCode(objs[r & 63]);
        }
        return acc;
    }

    static long sbLength(StringBuilder sb, int rounds) {
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            acc += sb.length();
        }
        return acc;
    }

    static long sbAppendChar(StringBuilder sb, int rounds) {
        for (int r = 0; r < rounds; r++) {
            sb.append('x');
        }
        return sb.length();
    }

    static long sbAppendInt(StringBuilder sb, int rounds) {
        for (int r = 0; r < rounds; r++) {
            sb.append(7);
        }
        return sb.length();
    }

    static long sbSetLength(StringBuilder sb, int rounds) {
        for (int r = 0; r < rounds; r++) {
            sb.setLength(0);
        }
        return sb.length();
    }

    static long strLength(String[] ss, int rounds) {
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            acc += ss[r & 63].length();
        }
        return acc;
    }

    static void rung(String name, int ops, long t0, long t1, long c) {
        System.out.printf("%-24s %8d ms  %9.1f ns/op   [%d]%n",
                          name, (t1 - t0), (t1 - t0) * 1e6 / ops, c);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 4;

        Object[] objs = new Object[64];
        for (int i = 0; i < objs.length; i++) {
            objs[i] = new Object();
        }
        String[] ss = new String[64];
        for (int i = 0; i < ss.length; i++) {
            ss[i] = "s" + i;
        }

        for (int p = 1; p <= passes; p++) {
            System.out.println("-- pass " + p);
            long t0, t1, c;

            t0 = System.currentTimeMillis(); c = idHash(objs, n);   t1 = System.currentTimeMillis();
            rung("System.identityHashCode", n, t0, t1, c != 0 ? 1 : 0);

            t0 = System.currentTimeMillis(); c = strLength(ss, n);  t1 = System.currentTimeMillis();
            rung("String.length", n, t0, t1, c);

            // Pre-sized so nothing below ever grows the payload.
            StringBuilder sb = new StringBuilder(n + 64);
            sb.append('s');

            t0 = System.currentTimeMillis(); c = sbLength(sb, n);   t1 = System.currentTimeMillis();
            rung("StringBuilder.length", n, t0, t1, c);

            t0 = System.currentTimeMillis(); c = sbSetLength(sb, n); t1 = System.currentTimeMillis();
            rung("StringBuilder.setLength", n, t0, t1, c);

            t0 = System.currentTimeMillis(); c = sbAppendChar(sb, n); t1 = System.currentTimeMillis();
            rung("StringBuilder.append(char)", n, t0, t1, c);

            sb.setLength(0);
            StringBuilder sb2 = new StringBuilder(n + 64);
            t0 = System.currentTimeMillis(); c = sbAppendInt(sb2, n); t1 = System.currentTimeMillis();
            rung("StringBuilder.append(int)", n, t0, t1, c);
        }
    }
}
