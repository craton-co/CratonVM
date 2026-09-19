import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** SrPhases3 — StringRegexOnly's three phases, timed in-process, plus a
 *  find()-only rung (no group(), no parseLong) to price find and group apart. */
public class SrPhases3 {

    static String build(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 1; i <= n; i++) { sb.append(i).append(' '); }
        return sb.toString();
    }

    static long findOnly(Matcher m) {
        m.reset();
        long k = 0;
        while (m.find()) { k++; }
        return k;
    }

    static long findGroup(Matcher m) {
        m.reset();
        long k = 0;
        while (m.find()) { k += m.group(1).length(); }
        return k;
    }

    static long findGroupParse(Matcher m) {
        m.reset();
        long sum = 0;
        while (m.find()) { sum += Long.parseLong(m.group(1)); }
        return sum;
    }

    static void rung(String name, int ops, long t0, long t1, long c) {
        System.out.printf("%-18s %8d ms  %9.1f ns/el   [%d]%n",
                          name, (t1 - t0), (t1 - t0) * 1e6 / ops, c);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        int passes = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        for (int p = 1; p <= passes; p++) {
            System.out.println("-- pass " + p);
            long t0, t1, c;
            t0 = System.currentTimeMillis(); String s = build(n); t1 = System.currentTimeMillis();
            rung("build", n, t0, t1, s.length());
            Pattern pat = Pattern.compile("(\\d+)");
            Matcher m = pat.matcher(s);
            t0 = System.currentTimeMillis(); c = findOnly(m);        t1 = System.currentTimeMillis();
            rung("find", n, t0, t1, c);
            t0 = System.currentTimeMillis(); c = findGroup(m);       t1 = System.currentTimeMillis();
            rung("find+group", n, t0, t1, c);
            t0 = System.currentTimeMillis(); c = findGroupParse(m);  t1 = System.currentTimeMillis();
            rung("find+group+parse", n, t0, t1, c);
        }
    }
}
