import java.util.function.IntUnaryOperator;

/**
 * WHICH part of a lambda call is the 8–10x?
 *
 * `CompositionPrimitivesProbe` established that `Function.apply` costs
 * ~1.7–2.1 µs on CratonVM against 209–236 ns on HotSpot's INTERPRETER, while a
 * plain static call on the same VM is 2–4x FASTER than HotSpot's interpreter.
 * That probe cannot say WHY, because `Function<Integer,Integer>.apply` bundles
 * three separable things:
 *
 *   1. autoboxing on both sides of the call,
 *   2. `invokeinterface` rather than `invokestatic`,
 *   3. the receiver being a LAMBDA PROXY rather than an ordinary class.
 *
 * This probe separates all three. Everything below uses `IntUnaryOperator`, so
 * no row boxes; the ladder is otherwise identical, one `int -> int` call per
 * op, same loop shape in a called method.
 *
 *   staticCall  invokestatic, the control that says the JIT works at all
 *   virtualCall invokevirtual on an ordinary final-ish class
 *   ifaceClass  invokeinterface, receiver is a NAMED class implementing the SAM
 *   ifaceAnon   invokeinterface, receiver is an ANONYMOUS class
 *   ifaceLambda invokeinterface, receiver is a LAMBDA (invokedynamic-created)
 *   ifaceMref   invokeinterface, receiver is a METHOD REFERENCE
 *   ifaceCap    invokeinterface, receiver is a CAPTURING lambda
 *   boxedLambda `Function<Integer,Integer>` — the original shape, for the bridge
 *
 * Read the ladder, not any single row:
 *   * `ifaceClass` slow too  => the cost is `invokeinterface`, not lambdas.
 *   * only the lambda rows slow => the cost is the lambda-proxy machinery
 *     (`try_lambda_dispatch` / `coerce_lambda_args` / the `lambda_proxies` map).
 *   * `boxedLambda` >> `ifaceLambda` => boxing is a separate, additive cost.
 *
 * Compare against HotSpot `-Xint`, never C2: the project's yardstick is "2.5x
 * versus -Xint is the statement about this VM".
 */
public class SamDispatchDecompositionProbe {

    private static final int WARMUP = 50_000;
    private static final int ITERS = 500_000;

    private static int sink;
    private static Object osink;

    // --- receivers -------------------------------------------------------
    static final class NamedOp implements IntUnaryOperator {
        @Override
        public int applyAsInt(int v) {
            return v + 1;
        }
    }

    static class PlainClass {
        int add1(int v) {
            return v + 1;
        }
    }

    private static int addStatic(int v) {
        return v + 1;
    }

    private static final NamedOp NAMED = new NamedOp();
    private static final PlainClass PLAIN = new PlainClass();

    private static final IntUnaryOperator ANON = new IntUnaryOperator() {
        @Override
        public int applyAsInt(int v) {
            return v + 1;
        }
    };
    private static final IntUnaryOperator LAMBDA = v -> v + 1;
    private static final IntUnaryOperator MREF = SamDispatchDecompositionProbe::addStatic;

    private static IntUnaryOperator capturing(int k) {
        return v -> v + k;   // captures k, so a fresh proxy instance per call
    }

    private static final IntUnaryOperator CAPTURED = capturing(1);

    private static final java.util.function.Function<Integer, Integer> BOXED = v -> v + 1;

    // --- shapes ----------------------------------------------------------
    private static long staticCall(int n) {
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += addStatic(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static long virtualCall(int n) {
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += PLAIN.add1(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    /** invokeinterface, but the receiver is an ordinary named class. */
    private static long ifaceClass(int n) {
        IntUnaryOperator op = NAMED;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.applyAsInt(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static long ifaceAnon(int n) {
        IntUnaryOperator op = ANON;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.applyAsInt(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static long ifaceLambda(int n) {
        IntUnaryOperator op = LAMBDA;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.applyAsInt(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static long ifaceMref(int n) {
        IntUnaryOperator op = MREF;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.applyAsInt(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static long ifaceCap(int n) {
        IntUnaryOperator op = CAPTURED;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.applyAsInt(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    /** The original `CompositionPrimitivesProbe` shape: same call, but boxed. */
    private static long boxedLambda(int n) {
        java.util.function.Function<Integer, Integer> op = BOXED;
        long t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc += op.apply(i);
        }
        sink = acc;
        return System.nanoTime() - t0;
    }

    private static void report(String name, long nanos, int n) {
        System.out.printf("%-12s %9.1f ns/op%n", name, (double) nanos / n);
    }

    public static void main(String[] args) {
        staticCall(WARMUP);
        virtualCall(WARMUP);
        ifaceClass(WARMUP);
        ifaceAnon(WARMUP);
        ifaceLambda(WARMUP);
        ifaceMref(WARMUP);
        ifaceCap(WARMUP);
        boxedLambda(WARMUP);

        report("staticCall", staticCall(ITERS), ITERS);
        report("virtualCall", virtualCall(ITERS), ITERS);
        report("ifaceClass", ifaceClass(ITERS), ITERS);
        report("ifaceAnon", ifaceAnon(ITERS), ITERS);
        report("ifaceLambda", ifaceLambda(ITERS), ITERS);
        report("ifaceMref", ifaceMref(ITERS), ITERS);
        report("ifaceCap", ifaceCap(ITERS), ITERS);
        report("boxedLambda", boxedLambda(ITERS), ITERS);
        osink = LAMBDA;
        System.out.println("PROBE-DONE sink=" + sink + " " + (osink != null));
    }
}
