// Minimal repro for the "hot method never compiles if it contains
// `new <not-yet-loaded class>`" JIT coverage gap.
//
// `hot()` is unambiguously hot and its only allocation is on a branch that is
// never taken.  Because nothing else in the process ever loads `Cold`, the
// compile-time `new` resolver cannot name a class id for it.  Before the fix
// that returned "unresolvable", which bailed the WHOLE compile; after
// MAX_TIER_FAIL_RETRIES (3) attempts the method was never retried and stayed
// interpreted forever.
//
// Run with CRATONVM_DBG=jit-method-stats and compare `hot_but_stuck_in_
// interpreter` / `compile-failures` between builds.  `-Dcold.trip=true` flips
// the branch so `Cold` IS loaded first -- the control arm, which always
// compiled even before the fix.
public class ColdNewJitProbe {
    static final class Cold extends RuntimeException {
        Cold(String m) {
            super(m);
        }
    }

    static int hot(int x) {
        if (x < 0) {
            throw new Cold("never taken in the default arm");
        }
        int s = x;
        for (int i = 0; i < 32; i++) {
            s = s * 31 + (x ^ i);
        }
        return s;
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        // Control arm: load `Cold` up front so the `new` site IS resolvable at
        // compile time. Everything else about the run is identical.
        if (Boolean.getBoolean("cold.trip")) {
            try {
                hot(-1);
            } catch (Cold expected) {
                System.out.println("warmup: Cold loaded");
            }
        }
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < iterations; i++) {
            acc += hot(i);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("acc=" + acc + " iterations=" + iterations + " ms=" + ms);
    }
}
