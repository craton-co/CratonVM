/**
 * SpliceControlProbe -- SpliceStaticProbe's control arm.
 *
 * Identical shape and identical call count, with the three `getstatic`
 * accessors replaced by ones that read nothing. Every callee here is already
 * spliceable by the optimizing tier, so a difference between this probe and
 * SpliceStaticProbe is attributable to the splice and not to the call count,
 * the loop, or the tier thresholds.
 *
 * MEASURED 2026-09-09: 23-31 ms with the optimizing body against 29-36 ms
 * with the single-pass one, stable, no second mode. That is the control that
 * makes SpliceStaticProbe's result attributable to the splice: same call count
 * and same loop, and when every callee IS spliceable the optimizing tier wins.
 *
 * Usage: SpliceControlProbe [reps]     default 4,000,000
 */
public class SpliceControlProbe {
    static int scale(int x) { return x * 0x9E3779B1; }
    static int bias(int x) { return x + 1013904223; }
    static int drift(int x) { return x + 7; }

    static int mix(int x) {
        return bias(scale(x));
    }

    static int step(int acc, int i) {
        return drift(mix(acc ^ i));
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm = step(warm, i);
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(acc, i);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. splicecontrol (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
