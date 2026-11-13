// j27 wave 3 (lane BACKLOG, STR16-P7): the DecimalDigits thin helpers
// (CRATONVM_JIT_DECIMAL_DIGITS_DIRECT, default on) no longer count served /
// declined calls. The digits they write must not change: edge values, a hot
// append loop (compiled), and four threads converting at once (the case the
// shared counter cache line cost).
// Page: docs/feature-designs/jit-ir-optimizations.md (STR16-P7)
//
// Expected HotSpot 25 output (facts only):
//   fact int-edges -2147483648|-1000000000|-10|-9|-1|0|1|9|10|999999999|2147483647
//   fact long-edges -9223372036854775808|-10000000000|-1|0|1|10000000000|9223372036854775807
//   fact append-int-len 527784
//   fact append-int-hash 162182197
//   fact append-long-len 988887
//   fact threads-len 2168517
// (Values computed off-line from the Java definitions; confirm on the
// HotSpot reference run.)
// -Dj27.timing=true adds `time append-int-ns <n>` (not compared).
public class J27DecimalDigitsRows {
    static String intEdges() {
        int[] v = {Integer.MIN_VALUE, -1000000000, -10, -9, -1, 0, 1, 9, 10, 999999999, Integer.MAX_VALUE};
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < v.length; i++) {
            if (i > 0) sb.append('|');
            sb.append(Integer.toString(v[i]));
        }
        return sb.toString();
    }

    static String longEdges() {
        long[] v = {Long.MIN_VALUE, -10000000000L, -1L, 0L, 1L, 10000000000L, Long.MAX_VALUE};
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < v.length; i++) {
            if (i > 0) sb.append('|');
            sb.append(String.valueOf(v[i]));
        }
        return sb.toString();
    }

    static String appendInts(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) {
            sb.append(i - n / 2);
        }
        return sb.toString();
    }

    static String appendLongs(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) {
            sb.append((long) i * 100003L);
        }
        return sb.toString();
    }

    static long threadsLength() throws InterruptedException {
        final long[] lens = new long[4];
        Thread[] ts = new Thread[4];
        for (int t = 0; t < 4; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long len = 0;
                for (int i = 0; i < 100000; i++) {
                    len += Integer.toString(i * (id + 1)).length();
                }
                lens[id] = len;
            });
            ts[t].start();
        }
        long total = 0;
        for (int t = 0; t < 4; t++) {
            ts[t].join();
            total += lens[t];
        }
        return total;
    }

    public static void main(String[] args) throws Exception {
        // Warm the conversion paths so the rows below run compiled.
        for (int r = 0; r < 20; r++) {
            appendInts(5000);
            appendLongs(5000);
        }
        System.out.println("fact int-edges " + intEdges());
        System.out.println("fact long-edges " + longEdges());
        long t0 = System.nanoTime();
        String ints = appendInts(100000);
        long t1 = System.nanoTime();
        System.out.println("fact append-int-len " + ints.length());
        System.out.println("fact append-int-hash " + ints.hashCode());
        System.out.println("fact append-long-len " + appendLongs(100000).length());
        System.out.println("fact threads-len " + threadsLength());
        if (Boolean.getBoolean("j27.timing")) {
            System.out.println("time append-int-ns " + (t1 - t0));
        }
    }
}
