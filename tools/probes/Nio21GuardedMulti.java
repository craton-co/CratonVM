import java.util.Arrays;

/**
 * nio21 wave 7 lane L4: bimorphic and polymorphic profile-guarded sites whose bodies are over
 * HotSpot's MaxInlineSize (35 bytes), for `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP_MULTI`
 * (r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size-FIXED-20261012.md).
 *
 * Conformance (default): every row warms a call site in a loop on two (bi) or three (poly)
 * receiver classes, then hands the SAME site a fourth class (the miss edge), a receiver whose
 * body throws, and null (NPE). Prints answers only; the output must equal HotSpot 25 in both
 * modes and with the switch =0.
 *
 * Bench: -Dnio21.bench=1 prints `<row> <N> ns/op` for bi and poly (median of 5 timed passes
 * after 3 warm ones; -Dnio21.iters=N, default 2000000).
 */
public class Nio21GuardedMulti {
    interface Shape {
        int score(int[] a, int k);
    }

    /** Loop-free bodies of 78..105 bytes: none fits the cold cap, all fit a third of the hot one. */
    static final class Sq implements Shape {
        final int w;
        Sq(int w) { this.w = w; }
        public int score(int[] a, int k) {
            int x = a[k & 7] * w + (k >>> 3);
            int y = (x ^ (x >>> 7)) * 31 + w;
            if (y < 0) y = -y;
            int z = y % 1009 + a[(k + 1) & 7];
            if ((z & 1) == 0) z += a[(k + 2) & 7] * 3; else z -= a[(k + 3) & 7];
            return z + (x & 0xff);
        }
    }

    static final class Tri implements Shape {
        final int h;
        Tri(int h) { this.h = h; }
        public int score(int[] a, int k) {
            int b = a[(k + 4) & 7] + h;
            int c = (b * 17) ^ (k << 2);
            if (c > 1000) c -= 999; else c += a[(k + 5) & 7];
            int d = c * c % 4099 + (b >>> 2);
            return d - a[(k + 6) & 7] + (c & 15);
        }
    }

    static final class Hex implements Shape {
        final int s;
        Hex(int s) { this.s = s; }
        public int score(int[] a, int k) {
            int p = a[(k + 7) & 7] * 6 + s;
            int q = (p << 3) - (p >>> 1) + k % 13;
            if (q % 3 == 0) q = q / 3 + a[k & 7]; else q = q * 2 - s;
            return (q ^ 0x5a5a) + (p & 63);
        }
    }

    static final class Odd implements Shape {
        public int score(int[] a, int k) {
            return a[k & 7] - k;
        }
    }

    static final class Bad implements Shape {
        public int score(int[] a, int k) {
            return a[k + 100];
        }
    }

    static final int[] A = {3, 1, 4, 1, 5, 9, 2, 6};

    static int site(Shape s, int k) {
        return s.score(A, k);
    }

    static long run(Shape[] mix, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += site(mix[i % mix.length], i);
        }
        return acc;
    }

    static void tail(String row) {
        System.out.println(row + " miss " + site(new Odd(), 11));
        try {
            System.out.println(row + " bad " + site(new Bad(), 1));
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println(row + " bad " + e.getClass().getName() + ": " + e.getMessage());
        }
        try {
            System.out.println(row + " null " + site(null, 1));
        } catch (NullPointerException e) {
            System.out.println(row + " null " + e.getClass().getName());
        }
        System.out.println(row + " again " + run(MIXES.get(row), 1000));
    }

    static final java.util.Map<String, Shape[]> MIXES = new java.util.HashMap<>();

    static {
        MIXES.put("bi", new Shape[] {new Sq(3), new Tri(5)});
        MIXES.put("poly", new Shape[] {new Sq(7), new Tri(2), new Hex(9)});
    }

    public static void main(String[] args) {
        String rows = System.getProperty("nio21.rows", "bi,poly");
        boolean bench = "1".equals(System.getProperty("nio21.bench", ""));
        int iters = Integer.getInteger("nio21.iters", 2_000_000);
        for (String row : rows.split(",")) {
            Shape[] mix = MIXES.get(row);
            if (mix == null) continue;
            if (bench) {
                long[] t = new long[5];
                for (int w = 0; w < 3; w++) run(mix, iters);
                long sink = 0;
                for (int r = 0; r < 5; r++) {
                    long t0 = System.nanoTime();
                    sink += run(mix, iters);
                    t[r] = System.nanoTime() - t0;
                }
                Arrays.sort(t);
                System.out.println(row + " " + (t[2] / iters) + " ns/op");
                if (sink == 42) System.out.println("");
            } else {
                for (int w = 0; w < 20; w++) run(mix, 20_000);
                System.out.println(row + " warm " + run(mix, 100_000));
                tail(row);
            }
        }
    }
}
