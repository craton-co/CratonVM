import java.util.function.IntUnaryOperator;

/**
 * One shape per run, so a profile of it is a profile of THAT shape.
 *
 * SamDispatchDecompositionProbe runs eight rows in one process, and
 * `boxedLambda` — 4x the cost of every other row — owns most of the samples in
 * any profile taken over the whole thing. Reading such a profile as if it
 * described `ifaceLambda` is how a 40x gap gets attributed to whatever the
 * boxing row happened to be doing.
 *
 * Usage: SamHotLoopProbe &lt;shape&gt; [ops], shape in {lambda, mref, cap, klass}.
 * `klass` is the CONTROL: the identical interface call on an ordinary named
 * class, which this VM already serves in ~10 ns.
 */
public class SamHotLoopProbe {

    interface Op { int apply(int x); }

    static final class NamedOp implements Op {
        public int apply(int x) { return x + 1; }
    }

    private static int addOne(int x) { return x + 1; }

    private static Op capturing(int k) {
        return x -> x + k;
    }

    public static void main(String[] args) {
        String shape = args.length > 0 ? args[0] : "lambda";
        long ops = args.length > 1 ? Long.parseLong(args[1]) : 2_000_000L;

        Op op;
        switch (shape) {
            case "lambda": op = x -> x + 1; break;
            case "mref":   op = SamHotLoopProbe::addOne; break;
            case "cap":    op = capturing(1); break;
            case "klass":  op = new NamedOp(); break;
            default: throw new IllegalArgumentException(shape);
        }

        // Warm up, then measure. The loop body is deliberately trivial so the
        // dispatch is the measurement.
        long sink = run(op, 200_000);
        long start = System.nanoTime();
        sink += run(op, ops);
        long elapsed = System.nanoTime() - start;

        System.out.printf("%s %.1f ns/op  ops=%d sink=%d%n",
                shape, (double) elapsed / ops, ops, sink);
    }

    private static long run(Op op, long n) {
        long sink = 0;
        for (long i = 0; i < n; i++) {
            sink += op.apply((int) (i & 0xFFFF));
        }
        return sink;
    }
}
