// j27 batch 7: a scalar-intrinsic call inside a SPLICED callee.
// Page: docs/feature-designs/jit-ir-optimizations.md
//
// `ti()` calls `ci()` 200 times, so the optimizing tier splices `ci` into `ti`; the
// `Integer.compress` / `expand` (and `numberOfLeadingZeros`) calls inside `ci` must
// then lower as arithmetic, not as the JDK bodies. Facts are the checksums (every arm
// must equal HotSpot); `-Dj27.time=true` adds `time <row>-ns-per-element`.
//
//   java -Xmx256m J27SpliceScalarRows [reps]
public class J27SpliceScalarRows {
    static final int N = 1024;
    static final int[] X = new int[N], M = new int[N];
    static final long[] XL = new long[N], ML = new long[N];

    static int ci() {
        int s = 0;
        for (int i = 0; i < N; i++) s += Integer.compress(X[i], M[i]) ^ Integer.expand(X[i], M[i]);
        return s;
    }

    static long cl() {
        long s = 0;
        for (int i = 0; i < N; i++) s += Long.compress(XL[i], ML[i]) ^ Long.expand(XL[i], ML[i]);
        return s;
    }

    static int nz() {
        int s = 0;
        for (int i = 0; i < N; i++) s += Integer.numberOfLeadingZeros(X[i]) + Integer.numberOfTrailingZeros(M[i]);
        return s;
    }

    static int mm() {
        int s = 0;
        for (int i = 0; i < N; i++) s += Math.max(X[i], M[i]) - Math.min(X[i], M[i]);
        return s;
    }

    static long c1(long x, long m) { return Long.compress(x, m) ^ Long.expand(x, m); }

    static long cs() {
        long s = 0;
        for (int i = 0; i < N; i++) s += c1(XL[i], ML[i]);
        return s;
    }

    // Each timed body only calls the helper, so the helper is spliced into it.
    static long ti() { long s = 0; for (int r = 0; r < 200; r++) s += ci(); return s; }
    static long tl() { long s = 0; for (int r = 0; r < 200; r++) s += cl(); return s; }
    static long tn() { long s = 0; for (int r = 0; r < 200; r++) s += nz(); return s; }
    static long tm() { long s = 0; for (int r = 0; r < 200; r++) s += mm(); return s; }
    static long tc() { long s = 0; for (int r = 0; r < 200; r++) s += cs(); return s; }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        boolean time = Boolean.getBoolean("j27.time");
        long seed = 0x9E3779B97F4A7C15L;
        for (int i = 0; i < N; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            X[i] = (int) (seed >>> 20); M[i] = (int) (seed >>> 33) | (i % 7 == 0 ? 0 : 1);
            XL[i] = seed; ML[i] = seed ^ (seed >>> 17);
        }
        String[] names = {"compress-expand-int", "compress-expand-long", "nlz-ntz-int", "minmax-int", "compress-expand-long-small"};
        long[] sums = new long[5];
        long[] best = {Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE, Long.MAX_VALUE};
        for (int rep = 0; rep < reps; rep++) {
            for (int k = 0; k < 5; k++) {
                long t0 = System.nanoTime();
                long r = k == 0 ? ti() : k == 1 ? tl() : k == 2 ? tn() : k == 3 ? tm() : tc();
                long dt = System.nanoTime() - t0;
                sums[k] = r;
                if (rep >= reps / 2 && dt < best[k]) best[k] = dt;
            }
        }
        for (int k = 0; k < 5; k++) {
            System.out.println("fact " + names[k] + " " + sums[k]);
            if (time) System.out.println("time " + names[k] + "-ns-per-element " + best[k] / 200.0 / N);
        }
    }
}
