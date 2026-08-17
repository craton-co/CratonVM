/**
 * Cost of ONE `throw`/`catch` of a preallocated exception across a compiled
 * frame, in the tiered shape (each arm's method called REPS times).
 *
 * The exception overrides `fillInStackTrace` to return `this`, exactly as
 * netty's `HttpHeaderValidationUtilTest.VALIDATION_EXCEPTION` does, so a VM
 * that honours the override pays nothing for the trace.
 *
 *   `noThrow`   — the same loop, condition never true.
 *   `throwAll`  — throws and catches on every iteration.
 *   `throwSome` — throws on ~1 iteration in 13, the rate the netty test's
 *                 exhaustive value loop actually hits (7.7%).
 */
public final class ThrowCostProbe {
    static long sink;

    static final RuntimeException E = new RuntimeException() {
        @Override public synchronized Throwable fillInStackTrace() { return this; }
    };

    static void leaf(int i, int mod) { if (mod != 0 && (i % mod) == 0) { throw E; } }

    static void arm(int n, int mod) {
        for (int i = 1; i <= n; i++) {
            try { leaf(i, mod); } catch (RuntimeException e) { sink++; }
        }
    }

    interface Arm { void run(int n); }

    static void time(String name, Arm a, int n, int reps) {
        int per = n / reps;
        for (int w = 0; w < reps; w++) { a.run(per); }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) { a.run(per); }
        long t1 = System.nanoTime();
        System.out.printf("%-10s %10.2f ns/iter  (%d ms)%n", name, (double) (t1 - t0) / (per * (long) reps), (t1 - t0) / 1_000_000L);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        time("no-throw",   x -> arm(x, 0),  n, reps);
        time("throw-1/13", x -> arm(x, 13), n, reps);
        time("throw-all",  x -> arm(x, 1),  n, reps);
        System.out.println("sink=" + sink);
    }
}
