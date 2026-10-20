/**
 * SrSplit — separate "compiled Java code quality" from "native-boundary cost"
 * on the exact shape StringRegexOnly uses.
 *
 * buildNative : sb.append(int).append(char)  -> 2 intercepted natives/element
 * buildManual : the SAME digits, written by hand into a char[] -> 0 natives
 * parseNative : Long.parseLong(String)       -> 1 intercepted native/element
 * parseManual : the same parse, by hand off a char[]           -> 0 natives
 *
 * Same arithmetic, same loop trip count. The ratio between each pair is the
 * native boundary, with codegen held constant. Checksums printed.
 */
public class SrSplit {

    static long buildNative(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 1; i <= n; i++) {
            sb.append(i).append(' ');
        }
        return sb.length();
    }

    static long buildManual(int n) {
        char[] buf = new char[n * 8 + 16];
        int p = 0;
        char[] tmp = new char[12];
        for (int i = 1; i <= n; i++) {
            int v = i;
            int t = 12;
            do { tmp[--t] = (char) ('0' + v % 10); v /= 10; } while (v != 0);
            while (t < 12) { buf[p++] = tmp[t++]; }
            buf[p++] = ' ';
        }
        return p;
    }

    static long parseNative(String[] toks) {
        long sum = 0;
        for (int i = 0; i < toks.length; i++) {
            sum += Long.parseLong(toks[i]);
        }
        return sum;
    }

    static long parseManual(char[][] toks) {
        long sum = 0;
        for (int i = 0; i < toks.length; i++) {
            char[] t = toks[i];
            long v = 0;
            for (int j = 0; j < t.length; j++) { v = v * 10 + (t[j] - '0'); }
            sum += v;
        }
        return sum;
    }

    static void rung(String name, int ops, long t0, long t1, long c) {
        System.out.printf("%-16s %8d ms  %9.1f ns/el   [%d]%n",
                          name, (t1 - t0), (t1 - t0) * 1e6 / ops, c);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 3;

        int m = n / 4;
        String[] toks = new String[m];
        char[][] ctoks = new char[m][];
        for (int i = 0; i < m; i++) {
            toks[i] = Integer.toString(i + 1);
            ctoks[i] = toks[i].toCharArray();
        }

        for (int p = 1; p <= passes; p++) {
            System.out.println("-- pass " + p);
            long t0, t1, c;

            t0 = System.currentTimeMillis(); c = buildManual(n); t1 = System.currentTimeMillis();
            rung("buildManual", n, t0, t1, c);

            t0 = System.currentTimeMillis(); c = buildNative(n); t1 = System.currentTimeMillis();
            rung("buildNative", n, t0, t1, c);

            t0 = System.currentTimeMillis(); c = parseManual(ctoks); t1 = System.currentTimeMillis();
            rung("parseManual", m, t0, t1, c);

            t0 = System.currentTimeMillis(); c = parseNative(toks); t1 = System.currentTimeMillis();
            rung("parseNative", m, t0, t1, c);
        }
    }
}
