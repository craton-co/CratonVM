// Scaled vector-add probe to break down the per-phase cost.
// Usage: java VAddProbe <phase> <log2n>
//   phase: alloc | fill | vadd | sum | full
// Prints: RESULT phase=<p> n=<n> ms=<t> checksum=<c>
public class VAddProbe {
    static void vadd(int[] a, int[] b, int[] c, int n) {
        for (int i = 0; i < n; i++) c[i] = a[i] + b[i];
    }
    static void fill(int[] a, int[] b, int n) {
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = 2 * i; }
    }
    static long sum(int[] c, int n) {
        long checksum = 0;
        for (int i = 0; i < n; i++) checksum += c[i];
        return checksum;
    }
    public static void main(String[] args) {
        String phase = args.length > 0 ? args[0] : "full";
        int log2n = args.length > 1 ? Integer.parseInt(args[1]) : 24;
        int n = 1 << log2n;
        long t0 = System.currentTimeMillis();
        long checksum = 0;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] c = new int[n];
        if (phase.equals("alloc")) {
            // touch one element so allocation isn't dead-code-eliminated
            checksum = a[0] + b[0] + c[0];
        } else if (phase.equals("fill")) {
            fill(a, b, n);
            checksum = a[n-1] + b[n-1];
        } else if (phase.equals("vadd")) {
            fill(a, b, n);
            long tv = System.currentTimeMillis();
            vadd(a, b, c, n);
            long ms2 = System.currentTimeMillis() - tv;
            checksum = c[n-1];
            System.out.println("RESULT phase=vadd-only n=" + n + " ms=" + ms2 + " checksum=" + checksum);
            return;
        } else if (phase.equals("sum")) {
            fill(a, b, n);
            vadd(a, b, c, n);
            long ts = System.currentTimeMillis();
            checksum = sum(c, n);
            long ms2 = System.currentTimeMillis() - ts;
            System.out.println("RESULT phase=sum-only n=" + n + " ms=" + ms2 + " checksum=" + checksum);
            return;
        } else if (phase.equals("inline")) {
            // Mirror BenchSuite.vectorAdd exactly: fill + sum inline in this method,
            // only vadd is a separate call.
            for (int i = 0; i < n; i++) { a[i] = i; b[i] = 2 * i; }
            vadd(a, b, c, n);
            long cs = 0;
            for (int i = 0; i < n; i++) cs += c[i];
            checksum = cs;
        } else { // full
            fill(a, b, n);
            vadd(a, b, c, n);
            checksum = sum(c, n);
        }
        long ms = System.currentTimeMillis() - t0;
        System.out.println("RESULT phase=" + phase + " n=" + n + " ms=" + ms + " checksum=" + checksum);
    }
}
