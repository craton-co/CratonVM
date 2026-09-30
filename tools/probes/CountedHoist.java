/**
 * The loop the LICM speculation permission refuses, and the one shape where
 * refusing it is provably unnecessary.
 *
 * `walk` is STATIC, so its base is a parameter that may be null, and
 * `probes/ZeroTripHoist.java` is why a hoist out of a loop that might not run
 * is a wrong answer rather than an optimization. But this loop's bound is a
 * CONSTANT: it runs 1000 times or the method is not entered at all, so the
 * first iteration was always going to dereference `o`, and hoisting its read to
 * the pre-header cannot invent a fault. `CRATONVM_JIT_IR_LICM_HOIST_COUNTED`
 * is the switch that lets LICM believe that.
 *
 * 1000 rather than 8 because the full unroller is default-ON and takes small
 * constant-trip loops apart before LICM sees them — the permission only ever
 * reaches a constant-trip loop too big to fully unroll, and this probe is that
 * loop.
 *
 * `walkNull` is the safety half, in the same file so the two cannot drift: the
 * SAME method shape with a zero-trip bound, called with null. It must return 0.
 */
public class CountedHoist {
    static class N {
        int v = 3;
    }

    static int walk(N o) {
        int a = 0;
        for (int i = 0; i < 1000; i++) {
            a += o.v;
        }
        return a;
    }

    /** A parameter bound, so nothing proves the body runs. `walkNull(null, 0)` is 0. */
    static int walkVar(N o, int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            a += o.v;
        }
        return a;
    }

    public static void main(String[] args) {
        N live = new N();
        int reps = Integer.getInteger("probe.reps", 20000);
        long acc = 0;
        for (int r = 0; r < 50; r++) {
            acc += walk(live);
        }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            acc += walk(live);
        }
        long t1 = System.nanoTime();
        // The safety half: a variable-bound loop over a null base, zero trips.
        int zero;
        try {
            zero = walkVar(null, 0);
        } catch (NullPointerException e) {
            System.out.println("acc=-1 ms=0 VERDICT=SPECULATED-NPE");
            return;
        }
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L
                + " VERDICT=" + (zero == 0 ? "OK" : "WRONG"));
    }
}
