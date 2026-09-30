// Cost of H2 StringUtils.quoteStringSQL (a String.codePointAt loop) against plain charAt+append. Classpath: <h2 target/classes>.
//
// Record: nonpassed-classbyclass-census-RESOLVED-20260923.md (D7)
// Run: cratonvm --java-home <jdk-25> -c <dir>[:<classpath>] QuoteProbe2 <chars>
// Compare with the same command on HotSpot (java -cp ...).
public class QuoteProbe2 {
    public static void main(String[] a) throws Exception {
        int n = Integer.parseInt(a[0]);
        long t0 = System.nanoTime();
        String s = " ".repeat(n);
        long t1 = System.nanoTime();
        System.out.printf("repeat(%d)=%dms%n", n, (t1 - t0) / 1000000);
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < n; i++) b.append(' ');
        long t2 = System.nanoTime();
        System.out.printf("append x%d=%dms%n", n, (t2 - t1) / 1000000);
        StringBuilder q = new StringBuilder();
        for (int i = 0; i < n; i++) { char c = s.charAt(i); if (c == '\'') q.append(c); q.append(c); }
        long t3 = System.nanoTime();
        System.out.printf("charAt+append x%d=%dms%n", n, (t3 - t2) / 1000000);
        StringBuilder h = new StringBuilder();
        org.h2.util.StringUtils.quoteStringSQL(h, s);
        long t4 = System.nanoTime();
        System.out.printf("h2 quote x%d=%dms len=%d%n", n, (t4 - t3) / 1000000, h.length());
    }
}
