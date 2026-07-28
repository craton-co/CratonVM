/*
 * TryCatchC2Probe — isolates the cost of an exception table on a JIT-compiled
 * method, with no Tomcat/JUnit fixture.
 *
 * Background (docs/known-issues/tomcat/04-embedded-server-throughput-wall):
 * `jit/src/lib.rs` used to refuse the optimizing IR ("C2") pipeline for ANY
 * method with a non-empty exception table, so every `try`/`catch` method in
 * every workload was permanently single-pass-backend quality.
 *
 * Each pair below is the SAME arithmetic; the only difference is a try/catch
 * wrapped around it. The catch is never entered, so any ratio far from 1.0 is
 * a pure static compilation-quality gap.
 *
 * No lambdas / method references: invokedynamic is refused by the IR pipeline
 * on its own, and a shared dispatch site would add a constant to both arms.
 *
 * Usage:  cratonvm -cp <dir> TryCatchC2Probe [iters]
 */
public class TryCatchC2Probe {

    // ── pair 1: plain arithmetic loop body ───────────────────────────────
    static int plain(int a, int b) {
        int acc = 0;
        for (int i = 0; i < 32; i++) {
            acc += (a ^ (b + i)) * 3;
            acc ^= acc >>> 7;
        }
        return acc;
    }

    static int tryCatch(int a, int b) {
        int acc = 0;
        try {
            for (int i = 0; i < 32; i++) {
                acc += (a ^ (b + i)) * 3;
                acc ^= acc >>> 7;
            }
        } catch (RuntimeException e) {
            // Reads ONLY a parameter, so the RBC.6 "handler reads an unsafe
            // local" gate never fires — this is the params-only population.
            return a;
        }
        return acc;
    }

    // ── pair 2: a call inside the protected range (the ordinary shape) ───
    static int helper(int x) {
        return (x & 1) == 0 ? x + 1 : x - 1;
    }

    static int plainCall(int a, int b) {
        int acc = 0;
        for (int i = 0; i < 32; i++) {
            acc += helper(a ^ (b + i));
        }
        return acc;
    }

    static int tryCatchCall(int a, int b) {
        int acc = 0;
        try {
            for (int i = 0; i < 32; i++) {
                acc += helper(a ^ (b + i));
            }
        } catch (RuntimeException e) {
            return b;
        }
        return acc;
    }

    static int sink;

    static double runPlain(int iters) {
        int s = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += plain(i, i + 1); }
        long dt = System.nanoTime() - t0;
        sink += s;
        return (double) dt / iters;
    }

    static double runTryCatch(int iters) {
        int s = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += tryCatch(i, i + 1); }
        long dt = System.nanoTime() - t0;
        sink += s;
        return (double) dt / iters;
    }

    static double runPlainCall(int iters) {
        int s = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += plainCall(i, i + 1); }
        long dt = System.nanoTime() - t0;
        sink += s;
        return (double) dt / iters;
    }

    static double runTryCatchCall(int iters) {
        int s = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += tryCatchCall(i, i + 1); }
        long dt = System.nanoTime() - t0;
        sink += s;
        return (double) dt / iters;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200000;

        // Three rounds so a cold first round cannot dominate the verdict.
        for (int round = 1; round <= 3; round++) {
            double p1 = runPlain(iters);
            double t1 = runTryCatch(iters);
            double p2 = runPlainCall(iters);
            double t2 = runTryCatchCall(iters);
            System.out.println(String.format(
                    "round %d  plain=%.0f tryCatch=%.0f (%.2fx) | plainCall=%.0f tryCatchCall=%.0f (%.2fx)  ns/op",
                    round, p1, t1, t1 / p1, p2, t2, t2 / p2));
        }
        System.out.println("sink=" + sink);
    }
}
