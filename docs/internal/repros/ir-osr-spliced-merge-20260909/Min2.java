/** Caller invoked far above c2_threshold with a SHORT inner loop, so it tiers
 *  up by invocation count (method-entry C2 body) rather than by OSR. */
public class Min2 {
    static int clamp(int x, int lo, int hi) {
        int r;
        if (x < lo) r = lo; else if (x > hi) r = hi; else r = x;
        return r;
    }
    static long body(int seed) {
        long s = 0; int x = seed;
        for (int i = 0; i < 8; i++) { x = x*1103515245+12345; s += clamp(x>>>20, 100, 900); }
        return s;
    }
    public static void main(String[] a) {
        int reps = Integer.parseInt(a[0]);
        long s = 0;
        for (int r = 0; r < reps; r++) s += body(r);
        System.out.println("body=[" + s + "]");
    }
}
