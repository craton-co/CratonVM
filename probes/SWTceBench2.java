package cratonvm;

/**
 * What is the self-tail-call elimination worth at DEFAULT settings for the
 * population that keeps it — a method the C1→C2 supersede refuses?
 *
 * `c2_upgrade_would_engage` rejects any method with a `new`, so `tailAlloc`
 * (which allocates once, at the base case) never gets an optimizing body and
 * stays on the C1 lowering for the life of the process. No tier lever is set
 * here: this is the production configuration, A/B'd on
 * `CRATONVM_JIT_SELF_TAILCALL` alone.
 *
 * `nonTailAlloc` is the same work with the call out of tail position — never
 * eliminable in either arm, so its arm-to-arm delta is the noise floor.
 */
public final class SWTceBench2 {
    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 1000;
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200_000;

        long sink = 0;
        for (int i = 0; i < 4000; i++) {
            sink += ((int[]) tailAlloc(depth, 0))[0];
            sink += ((int[]) nonTailAlloc(depth))[0];
        }

        String engaged;
        try {
            tailAlloc(2_000_000, 0);
            engaged = "ELIMINATED";
        } catch (StackOverflowError e) {
            engaged = "FRAMES";
        }

        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            sink += ((int[]) tailAlloc(depth, i))[0];
        }
        long tailNs = System.nanoTime() - t0;

        long t1 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            sink += ((int[]) nonTailAlloc(depth))[0];
        }
        long nonTailNs = System.nanoTime() - t1;

        double levels = (double) iters * depth;
        System.out.printf(
                "engaged=%s depth=%d iters=%d tailAlloc=%.3f ns/level nonTailAlloc=%.3f ns/level sink=%d%n",
                engaged, depth, iters, tailNs / levels, nonTailNs / levels, sink);
    }

    /** Allocates at the base case, so the C2 supersede refuses it forever. */
    static Object tailAlloc(int d, int acc) {
        if (d == 0) {
            return new int[] {acc};
        }
        return tailAlloc(d - 1, acc + d);
    }

    /** Same, but the call is not in tail position. */
    static Object nonTailAlloc(int d) {
        if (d == 0) {
            return new int[] {0};
        }
        int[] r = (int[]) nonTailAlloc(d - 1);
        r[0] += d;
        return r;
    }
}
