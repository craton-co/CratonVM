// j27 wave 2 (lane NATMISC, REFLECT2-21-2): a helper whose body calls
// Math.max/min, spliced into a hot single-pass caller, keeps the CMOV / minmax.
// Likewise getClass(), Integer.valueOf(), Long.valueOf() spliced bodies.
// Answers must equal HotSpot's either way.
// Page: docs/known-issues/jit/jr21-native-shadow-refusals-on-intrinsified-calls-are-not-the-reflect-cost-20261014.md
public class J27SpliceMinMax {
    static int f(int a, int b) { return Math.max(a, b) + 1; }
    static long g(long a, long b) { return Math.min(a, b) - 1; }
    static int h(int a, int b) { return Math.min(a, b) * 2; }
    static float mf(float a, float b) { return Math.max(a, b) + 0.5f; }
    static double md(double a, double b) { return Math.min(a, b) - 0.5; }
    static Class<?> gc(Object o) { return o.getClass(); }
    static Integer iv(int a) { return Integer.valueOf(a); }
    static Long lv(long a) { return Long.valueOf(a); }

    public static void main(String[] args) {
        long s1 = 0, s2 = 0;
        int s3 = 0;
        float s4 = 0;
        double s5 = 0;
        int s6 = 0;
        long s7 = 0;
        long s8 = 0;
        String sample = "sample";
        for (int i = 0; i < 1_000_000; i++) {
            s1 += f(i % 7 - 3, 1);
            s2 += g(-(i % 5), -2);
            s3 += h(-7, 4);
            s4 += mf((i % 7) - 3.0f, 1.0f);
            s5 += md(-(i % 5) * 1.0, -2.0);
            s6 += gc(sample).hashCode();
            s7 += iv(i & 0x7f).intValue();
            s8 += lv((long)(i & 0x7f)).longValue();
        }
        System.out.println("fact max-int-sum " + s1);
        System.out.println("fact min-long-sum " + s2);
        System.out.println("fact min-int-neg-sum " + s3);
        System.out.println("fact max-float-sum " + (long)s4);
        System.out.println("fact min-double-sum " + (long)s5);
        System.out.println("fact getclass-hash-nonzero " + (s6 != 0));
        System.out.println("fact int-value-of-sum " + s7);
        System.out.println("fact long-value-of-sum " + s8);
        if (Boolean.getBoolean("j27.time")) {
            long t0 = System.nanoTime();
            long s = 0;
            for (int i = 0; i < 10_000_000; i++) s += f(i, 5_000_000);
            System.out.println("time splice-max-ns-per-op "
                    + (System.nanoTime() - t0) / 10_000_000.0 + " " + s);
        }
    }
}
