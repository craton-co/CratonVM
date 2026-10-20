/**
 * C2PartialUnrollProbe -- the loop shape the partial unroller serves.
 *
 * The census in `c2-unrolling-is-a-deopt-metadata-problem-20260911.md` found
 * that EVERY counted loop in CratonBench and CratonBenchC2 has a RUNTIME
 * bound, which is the population full unrolling can never serve. This probe is
 * that shape and nothing else: a `for (i = 0; i < n; i++)` whose body is pure
 * integer arithmetic over the induction variable, in a small static method
 * invoked often enough to be promoted to the optimizing tier.
 *
 * The method is small and hot so the TIER MANAGER promotes it (it promotes on
 * invocation count, `c2_threshold` = 20,000), and the inner trip count is small
 * so the measurement is dominated by the loop body rather than by call
 * overhead -- but not so small that the loop never runs.
 *
 * What the two arms compare, with everything else held fixed:
 *
 *   CRATONVM_JIT_IR_PARTIAL_UNROLL=0   one body per back edge + safepoint poll
 *   CRATONVM_JIT_IR_PARTIAL_UNROLL=1   `factor` bodies per back edge + poll
 *
 * The checksum is printed and must be IDENTICAL on both arms and against
 * HotSpot. A faster wrong answer is a bug, not a result -- and for this
 * transform specifically, the failure mode is an off-by-one in the number of
 * iterations run, which a checksum over every iteration's contribution catches
 * and a timing comparison does not.
 *
 * Usage: C2PartialUnrollProbe [reps] [trip]   default 2,000,000 and 24
 */
public class C2PartialUnrollProbe {

    /**
     * The loop under test. Runtime bound, constant stride, pure body.
     *
     * `a` and `i` are both carried, so the body is entirely loop-variant and
     * therefore entirely clonable -- which is what the unroller's own
     * clone-set walk requires.
     */
    static int sum(int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            a += i ^ (a >>> 3);
        }
        return a;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int trip = args.length > 1 ? Integer.parseInt(args[1]) : 24;

        // Warm past the optimizing tier's invocation threshold before timing.
        int warm = 0;
        for (int i = 0; i < 200_000; i++) warm += sum(trip);

        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc += sum(trip);
        long ms = (System.nanoTime() - t0) / 1_000_000L;

        System.out.println("1. c2partialunroll (" + reps + "x" + trip + ") : "
                + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
