package cratonvm;

/**
 * What is the single-pass self-tail-call elimination actually worth?
 *
 * A/B `CRATONVM_JIT_SELF_TAILCALL` inside ONE binary, with both arms pinned to
 * the C1 body (`CRATONVM_TIER_C2_THRESHOLD=100000000`) so the only difference
 * is the lowering: `JMP` back to the body entry versus a real self `CALL`.
 *
 *   tail    `(II)I`, self-call followed by `ireturn` -> eliminable
 *   nonTail `(I)I` shape but the call is not in tail position -> never
 *           eliminable, so its arm-to-arm delta is this harness's noise floor
 *
 * ENGAGED/FRAMES is printed from a deep drive of the same compiled body, so the
 * number never stands without the evidence that the lowering was on.
 */
public final class SWTceBench {
    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 1000;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 400_000;

        long sink = 0;
        for (int i = 0; i < 4000; i++) {
            sink += tail(depth, 0);
            sink += nonTail(depth);
        }

        String engaged;
        try {
            tail(2_000_000, 0);
            engaged = "ELIMINATED";
        } catch (StackOverflowError e) {
            engaged = "FRAMES";
        }

        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            sink += tail(depth, i);
        }
        long tailNs = System.nanoTime() - t0;

        long t1 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            sink += nonTail(depth);
        }
        long nonTailNs = System.nanoTime() - t1;

        double levels = (double) iters * depth;
        System.out.printf(
                "engaged=%s depth=%d iters=%d tail=%.3f ns/level nonTail=%.3f ns/level sink=%d%n",
                engaged, depth, iters, tailNs / levels, nonTailNs / levels, sink);
    }

    /** Self-call in tail position: `invokestatic` then `ireturn`. */
    static int tail(int d, int acc) {
        if (d == 0) {
            return acc;
        }
        return tail(d - 1, acc + d);
    }

    /** Same work, but the call is not in tail position — never eliminable. */
    static int nonTail(int d) {
        if (d == 0) {
            return 0;
        }
        return nonTail(d - 1) + d;
    }
}
