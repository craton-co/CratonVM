package cratonvm;

import java.util.function.*;

/**
 * The correctness gate for the lambda SAM JIT tier-up fast path.
 *
 * <p>A lambda implementation method reached through {@code try_lambda_dispatch}
 * used to touch neither the JIT's invocation counter nor its code cache, so it
 * could never be nominated for compilation however hot it got. Fixing that
 * means a lambda body is now entered through a DIRECT compiled call
 * ({@code jit_bridge::execute_jit_call_oneshot}) once it has been compiled —
 * a call that leaves the interpreter entirely and has to reproduce, by hand,
 * everything the interpreted frame path did for free.
 *
 * <p>Each {@code *Checksum} below is one thing that path can get wrong and the
 * interpreted path cannot. Every one loops far past the default hotness
 * threshold (500) so its body really is compiled for most of the run, and every
 * one folds its answer into a single {@code int} whose golden value came from
 * running this same file under a real JDK.
 *
 * <p>The exception arms matter most. A lambda body whose {@code throw} sits on
 * a cold branch compiles that branch to an uncommon trap, so the FIRST throwing
 * call after compilation deoptimizes — and it was exactly that combination
 * (compiled body, then a throw) that crashed the first attempt at this fix with
 * an operand-stack index underflow, thousands of calls later, inside an
 * unrelated interpreted execution of the same body. See
 * known-issues/perf/lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md
 * section 5.3. Any regression of that kind shows up here as a crash or a wrong
 * checksum, not as a slow test.
 */
public final class LambdaJitTierUp {

    private static final int N = 4000;

    private LambdaJitTierUp() {}

    private static int addOne(int x) {
        return x + 1;
    }

    /** Cold-branch throw: the shape that deoptimizes on its first throwing call. */
    private static final IntUnaryOperator MAYBE_THROW = v -> {
        if (v == 999) {
            throw new RuntimeException("boom");
        }
        return v * 2;
    };

    private static final IntUnaryOperator DIVIDER = v -> 1000 / (v % 500);

    private static final IntUnaryOperator INDEXER = v -> {
        int[] a = new int[4];
        return a[v % 8];
    };

    /** Carries its own exception table: the fast path must DECLINE this one. */
    private static final IntUnaryOperator SELF_CATCHING = v -> {
        try {
            if (v % 7 == 0) {
                throw new IllegalStateException("x");
            }
            return v;
        } catch (IllegalStateException e) {
            return -1;
        }
    };

    /** Non-capturing lambda: the plain int-returning fast path. */
    public static int plainChecksum() {
        IntUnaryOperator plus1 = v -> v + 1;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += plus1.applyAsInt(i);
        }
        return sum;
    }

    /** Capturing lambda: captures are prepended to the impl args. */
    public static int capturingChecksum() {
        int k = 7;
        IntUnaryOperator capAdd = v -> v + k;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += capAdd.applyAsInt(i);
        }
        return sum;
    }

    /** Method reference: an InvokeStatic impl handle rather than a lambda body. */
    public static int methodRefChecksum() {
        IntUnaryOperator mref = LambdaJitTierUp::addOne;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += mref.applyAsInt(i);
        }
        return sum;
    }

    /**
     * The section 5.3 repro. The throwing input arrives LATE — after the body
     * is compiled — and then repeatedly, so state corrupted by the first throw
     * surfaces on the calls that follow rather than on the throw itself.
     */
    public static int throwingChecksum() {
        int sum = 0;
        int caught = 0;
        for (int i = 0; i < N; i++) {
            int v = (i > 2000 && i % 500 == 0) ? 999 : i;
            try {
                sum += MAYBE_THROW.applyAsInt(v);
            } catch (RuntimeException e) {
                caught++;
            }
        }
        return sum + caught * 31;
    }

    /** A default method on the functional interface, dispatched on a lambda. */
    public static int composedChecksum() {
        IntUnaryOperator base = v -> v + 100;
        IntUnaryOperator composed = base.andThen(v -> v * 2);
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += composed.applyAsInt(i);
        }
        return sum;
    }

    /**
     * Every arm of the one-shot's return-value conversion: long, double,
     * reference, and void (which must return no value at all, not a zero).
     */
    public static int returnShapesChecksum() {
        LongUnaryOperator lop = v -> v * 3_000_000_000L;
        DoubleUnaryOperator dop = v -> v / 3.0;
        IntFunction<String> sop = v -> "s" + (v % 3);
        long lsum = 0;
        double dsum = 0;
        int ssum = 0;
        for (int i = 0; i < N; i++) {
            lsum += lop.applyAsLong(i % 7);
            dsum += dop.applyAsDouble(i % 11);
            ssum += sop.apply(i).length();
        }
        int[] sink = new int[1];
        IntConsumer voidOp = v -> sink[0] += v;
        for (int i = 0; i < N; i++) {
            voidOp.accept(i % 5);
        }
        return (int) (lsum % 1_000_003) + (int) dsum + ssum + sink[0];
    }

    /** ArithmeticException raised inside the compiled body (the sig.arithmetic drain). */
    public static int arithmeticChecksum() {
        int sum = 0;
        int caught = 0;
        for (int i = 0; i < N; i++) {
            try {
                sum += DIVIDER.applyAsInt(i + 1);
            } catch (ArithmeticException e) {
                caught++;
            }
        }
        return sum + caught * 31;
    }

    /** NullPointerException raised inside the compiled body (the sig.npe drain). */
    public static int npeChecksum() {
        Function<String, Integer> lengthOf = s -> s.length();
        int sum = 0;
        int caught = 0;
        for (int i = 0; i < N; i++) {
            String s = (i % 700 == 0) ? null : "abc";
            try {
                sum += lengthOf.apply(s);
            } catch (NullPointerException e) {
                caught++;
            }
        }
        return sum + caught * 31;
    }

    /** ArrayIndexOutOfBounds raised inside the compiled body (the sig.aioobe drain). */
    public static int arrayIndexChecksum() {
        int sum = 0;
        int caught = 0;
        for (int i = 0; i < N; i++) {
            try {
                sum += INDEXER.applyAsInt(i);
            } catch (ArrayIndexOutOfBoundsException e) {
                caught++;
            }
        }
        return sum + caught * 31;
    }

    /** A body with its own handler — the fast path declines, the answer must not change. */
    public static int selfCatchingChecksum() {
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += SELF_CATCHING.applyAsInt(i);
        }
        return sum;
    }

    /** A lambda body that itself dispatches another lambda (re-entrant dispatch). */
    public static int nestedChecksum() {
        IntUnaryOperator inner = v -> v * 2;
        IntUnaryOperator outer = v -> inner.applyAsInt(v) + 1;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += outer.applyAsInt(i);
        }
        return sum;
    }

    /**
     * A throw from a warm lambda body caught two Java frames further out, so no
     * local routing can be involved: it must PROPAGATE out of the one-shot call
     * synchronously, exactly as the interpreted path's own `?` does.
     */
    public static int propagationChecksum() {
        int caught = 0;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            try {
                sum += indirect(MAYBE_THROW, (i > 3000 && i % 300 == 0) ? 999 : i);
            } catch (RuntimeException e) {
                caught++;
            }
        }
        return sum + caught * 31;
    }

    private static int indirect(IntUnaryOperator op, int v) {
        return level2(op, v);
    }

    private static int level2(IntUnaryOperator op, int v) {
        return op.applyAsInt(v);
    }
}
