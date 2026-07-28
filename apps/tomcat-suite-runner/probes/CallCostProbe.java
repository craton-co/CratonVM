/*
 * CallCostProbe - marginal cost of ONE invoke inside JIT-compiled code.
 *
 * Two bodies that differ only in how many calls they make (loop trip count
 * 8 vs 40), so the difference divided by 32 is the marginal per-call cost with
 * loop/arith overhead cancelled out. Four invoke kinds are measured:
 * invokestatic, invokevirtual (monomorphic), invokeinterface (monomorphic),
 * and an inlined-by-hand control with no call at all.
 *
 * Usage:  cratonvm -cp <dir> CallCostProbe [iters]
 */
public class CallCostProbe {

    interface Op { int apply(int x); }

    static final class Impl implements Op {
        public int apply(int x) { return (x & 1) == 0 ? x + 1 : x - 1; }
        int virt(int x)         { return (x & 1) == 0 ? x + 1 : x - 1; }
    }

    static int stat(int x) { return (x & 1) == 0 ? x + 1 : x - 1; }

    static final Impl IMPL = new Impl();
    static final Op   OP   = IMPL;

    static int loopNone(int a, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            int x = a ^ i;
            acc += (x & 1) == 0 ? x + 1 : x - 1;
        }
        return acc;
    }

    static int loopStatic(int a, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) { acc += stat(a ^ i); }
        return acc;
    }

    static int loopVirtual(int a, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) { acc += IMPL.virt(a ^ i); }
        return acc;
    }

    static int loopInterface(int a, int n) {
        int acc = 0;
        for (int i = 0; i < n; i++) { acc += OP.apply(a ^ i); }
        return acc;
    }

    static int sink;

    static double timeNone(int iters, int n) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += loopNone(i, n); }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }
    static double timeStatic(int iters, int n) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += loopStatic(i, n); }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }
    static double timeVirtual(int iters, int n) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += loopVirtual(i, n); }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }
    static double timeInterface(int iters, int n) {
        int s = 0; long t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) { s += loopInterface(i, n); }
        long dt = System.nanoTime() - t0; sink += s; return (double) dt / iters;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        for (int round = 1; round <= 3; round++) {
            double n8  = timeNone(iters, 8),  n40 = timeNone(iters, 40);
            double s8  = timeStatic(iters, 8), s40 = timeStatic(iters, 40);
            double v8  = timeVirtual(iters, 8), v40 = timeVirtual(iters, 40);
            double i8  = timeInterface(iters, 8), i40 = timeInterface(iters, 40);
            System.out.println(String.format(
                "round %d  marginal ns per iteration: none=%.1f static=%.1f virtual=%.1f interface=%.1f",
                round, (n40 - n8) / 32, (s40 - s8) / 32, (v40 - v8) / 32, (i40 - i8) / 32));
        }
        System.out.println("sink=" + sink);
    }
}
