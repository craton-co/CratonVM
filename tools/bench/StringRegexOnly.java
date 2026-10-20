import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * StringRegexOnly — isolated String/Regex benchmark (README row).
 * Build "1 2 3 ... n" with StringBuilder, extract integers with
 * Pattern/Matcher find()+group(1), sum them.
 *   n=100,000   -> 5000050000
 *   n=1,000,000 -> 500000500000
 * Kernel lives in a static method (NOT main): main contains an
 * invokedynamic string concat, which bails the whole-method OSR artifact
 * compile and silently OSR-denies it — the loop would run interpreted
 * forever (found 2026-07-17).
 */
public class StringRegexOnly {
    static long run(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 1; i <= n; i++) {
            sb.append(i).append(' ');
        }
        String s = sb.toString();
        Pattern p = Pattern.compile("(\\d+)");
        Matcher m = p.matcher(s);
        long sum = 0;
        while (m.find()) {
            sum += Long.parseLong(m.group(1));
        }
        return sum;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1_000_000;
        long t0 = System.currentTimeMillis();
        long sum = run(n);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("StringRegex " + n + ": " + elapsed + " ms  [" + sum + "]");
    }
}
