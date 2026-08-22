/**
 * Prices the compiled-caller -> INTERPRETED-callee transition.
 *
 * `loop` is a hot loop that gets OSR-compiled; `callee` is a trivial add. With
 * `CRATONVM_JIT_DENY=XferProbe.callee` the callee stays interpreted while the
 * caller is still compiled, so the delta against the undenied run is the
 * transition cost and nothing else. The `--nojit` arm is the both-interpreted
 * control: if compiled->interpreted is SLOWER than interpreted->interpreted,
 * the transition is costing more than the compilation is buying.
 */
public class XferProbe {
    static long sink;

    static int callee(int x) { return x + 1; }

    static void loop(int n) {
        for (int i = 0; i < n; i++) sink += callee(i);
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        loop(50000);
        long t0 = System.nanoTime();
        loop(n);
        long d = System.nanoTime() - t0;
        System.out.printf("xfer %8.1f ns/op sink=%d%n", (double) d / n, sink);
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
