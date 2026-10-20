/**
 * C2LeafCostProbe -- what an optimizing-tier body costs a TRIVIAL LEAF.
 *
 * `drift()` is `getstatic; ireturn` and nothing else. It is refused for
 * splicing (`ir-splice-static-field`) so it stays a real call, and it is hot
 * enough on its own to be promoted, so `CRATONVM_C2_ACCEPT=never` against
 * `=always` compares the leaf's C1 body against its C2 body with the caller
 * held fixed.
 *
 * The caller is `main`'s own loop rather than a helper, so exactly one method
 * besides the leaf can change tier between the arms.
 *
 * MEASURED 2026-09-09, and it REFUTES the hypothesis it was written for.
 * "The optimizing tier's frame and metadata make a one-instruction accessor
 * dramatically more expensive than C1's" predicts a large gap here and there
 * is none: ~21 ms optimizing against ~20 ms single-pass. A trivial leaf's
 * optimizing body is fine. Kept because the refutation is the useful part --
 * without it the next reader re-derives the same plausible wrong answer.
 *
 * Usage: C2LeafCostProbe [reps]     default 4,000,000
 */
public class C2LeafCostProbe {
    static int drift = 7;

    static int drift() { return drift; }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 4_000_000;
        int warm = 0;
        for (int i = 0; i < 3_000_000; i++) warm += drift() ^ i;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < reps; i++) acc += drift() ^ i;
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("1. c2leafcost (" + reps + ") : " + ms + " ms  [" + acc + "]");
        if (warm == 0x7FFFFFFF) System.out.println(warm);
    }
}
