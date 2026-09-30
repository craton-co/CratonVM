/**
 * SpliceStaticProbe -- the callee shape the optimizing tier refused as
 * `ir-splice-static-field`.
 *
 * `scale()` and `bias()` are the accessor-over-a-static-table shape the
 * ir-coverage survey found framework code to be mostly made of: 92 of its 273
 * events were `getstatic`. Before the rows were rebased, a callee containing
 * one was refused whole at resolution, so `mix` -- which is otherwise exactly
 * the straight-line arithmetic body the splicer exists for -- kept two real
 * calls per iteration.
 *
 * `step` is invoked far past the tier manager's 20,000-invocation C2 threshold,
 * which is what CratonBench's one-enormous-loop-per-method phases cannot do
 * (see CratonBenchC2's header). Deterministic and checksummed, like every other
 * harness in this directory: a fast wrong answer is a bug, not a result.
 *
 * The warm-up is 3,000,000 iterations and not the 200,000 that clears the
 * threshold, because clearing the threshold only ENQUEUES the compile. The
 * background compiler has to finish and publish before the timed loop starts,
 * and at 200,000 it often did not: the same binary measured 57 ms and 863 ms on
 * alternate runs with identical compile counts, which is one body or the other
 * being installed, not a distribution. A measurement that mixes two modes has
 * no median worth quoting.
 *
 * Usage: SpliceStaticProbe [reps]     default 4,000,000
 */
public class SpliceStaticProbe {
    // NON-final deliberately. `static final int` is folded to an `ldc` at the
    // call site by javac, so a `final` table would produce no `getstatic` at
    // all and the probe would measure nothing. A registry, a cache, a
    // configured limit -- the statics framework code actually reads through an
    // accessor -- are not compile-time constants.
    static int scaleTable = 0x9E3779B1;
    static int biasTable = 1013904223;
    static int drift = 7;

    // Each of these is a `getstatic` behind an accessor -- the whole point.
    static int scale() { return scaleTable; }
    static int bias() { return biasTable; }
    static int drift() { return drift; }

    static int mix(int x) {
        return x * scale() + bias();
    }

    static int step(int acc, int i) {
        int m = mix(acc ^ i);
        return m + drift();
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        // Warm past the tier manager's threshold before timing, so the
        // measured window is the compiled body and not the ramp.
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm = step(warm, i);
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc = step(acc, i);
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. splicestatic (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
