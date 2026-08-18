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

    /**
     * Long enough that the background compiler certainly publishes each impl
     * body and the fast path certainly enters it for the BULK of the run.
     *
     * This was 4 000, which crosses the tier-up threshold (500) but need not
     * outlast an ASYNCHRONOUS compile. Measured, it did not: with a deliberate
     * off-by-one planted in the one-shot's return-value conversion and the
     * implicit-NPE drain deleted outright, eleven of the twelve tests here
     * still passed — they had computed their answers in the interpreter and
     * agreed with HotSpot about a code path they never took. Only the arm that
     * happened to run late enough caught it. At 200 000 every arm fails when
     * the fast path is broken, which is the only property that makes any of
     * them worth running.
     */
    private static final int N = 200_000;

    private LambdaJitTierUp() {}

    /**
     * Every SAM call in this file goes through here, and that is deliberate.
     *
     * The two halves of the lambda tier-up serve different CALLERS: a compiled
     * caller's SAM call is answered by the JIT-side direct arm, an interpreted
     * caller's by the interpreter's one-shot. Which half a fixture exercises is
     * therefore decided by whether its calling frame is compiled — and a loop
     * sitting directly in a `*Checksum` body, called once, leaves that to an
     * OSR race the test cannot see or control.
     *
     * A one-line static hop is called once per iteration, so with inline
     * compilation (`CRATONVM_BG_COMPILE=0`) it compiles deterministically at
     * the invocation threshold and every later SAM call comes out of compiled
     * code. That is what lets `lambda_jit_tierup_tests` cover the JIT-side arm
     * and `lambda_jit_oneshot_tests` — which turns that arm off — cover the
     * interpreted one, with neither depending on a race.
     */
    private static int step(IntUnaryOperator op, int v) {
        return op.applyAsInt(v);
    }

    private static long stepLong(LongUnaryOperator op, long v) {
        return op.applyAsLong(v);
    }

    private static double stepDouble(DoubleUnaryOperator op, double v) {
        return op.applyAsDouble(v);
    }

    private static String stepObj(IntFunction<String> op, int v) {
        return op.apply(v);
    }

    private static void stepVoid(IntConsumer op, int v) {
        op.accept(v);
    }

    private static int stepFn(Function<String, Integer> op, String v) {
        return op.apply(v);
    }

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

    /** Divides by zero once every 500 calls — often enough to exercise the
     * `sig.arithmetic` drain, rare enough that the run stays short. */
    private static final IntUnaryOperator DIVIDER = v -> 1000 / (v % 500);

    /** Indexes out of range once every 500 calls (`sig.aioobe`). */
    private static final IntUnaryOperator INDEXER = v -> {
        int[] a = new int[4];
        return a[v % 500 == 499 ? 7 : v % 4];
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
            sum += step(plus1, i) + plus1.applyAsInt(i);
        }
        return sum;
    }

    /** Capturing lambda: captures are prepended to the impl args. */
    public static int capturingChecksum() {
        int k = 7;
        IntUnaryOperator capAdd = v -> v + k;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += step(capAdd, i) + capAdd.applyAsInt(i);
        }
        return sum;
    }

    /** Method reference: an InvokeStatic impl handle rather than a lambda body. */
    public static int methodRefChecksum() {
        IntUnaryOperator mref = LambdaJitTierUp::addOne;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += step(mref, i) + mref.applyAsInt(i);
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
            int v = (i > 2000 && i % 5000 == 0) ? 999 : i;
            try {
                sum += step(MAYBE_THROW, v) + MAYBE_THROW.applyAsInt(v);
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
            sum += step(composed, i) + composed.applyAsInt(i);
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
            lsum += lop.applyAsLong(i % 7) + stepLong(lop, i % 7);
            dsum += dop.applyAsDouble(i % 11) + stepDouble(dop, i % 11);
            ssum += sop.apply(i).length() + stepObj(sop, i).length();
            lsum %= 1_000_003;
        }
        int[] sink = new int[1];
        IntConsumer voidOp = v -> sink[0] += v;
        for (int i = 0; i < N; i++) {
            voidOp.accept(i % 5);
            stepVoid(voidOp, i % 5);
        }
        return (int) (lsum % 1_000_003) + (int) (dsum % 1_000_003) + ssum + sink[0];
    }

    /** ArithmeticException raised inside the compiled body (the sig.arithmetic drain). */
    public static int arithmeticChecksum() {
        int sum = 0;
        int caught = 0;
        for (int i = 0; i < N; i++) {
            try {
                sum += step(DIVIDER, i + 1) + DIVIDER.applyAsInt(i + 1);
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
            String s = (i % 500 == 499) ? null : "abc";
            try {
                sum += lengthOf.apply(s) + stepFn(lengthOf, s);
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
                sum += step(INDEXER, i) + INDEXER.applyAsInt(i);
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
            sum += step(SELF_CATCHING, i) + SELF_CATCHING.applyAsInt(i);
        }
        return sum;
    }

    /** A lambda body that itself dispatches another lambda (re-entrant dispatch). */
    public static int nestedChecksum() {
        IntUnaryOperator inner = v -> v * 2;
        IntUnaryOperator outer = v -> inner.applyAsInt(v) + 1;
        int sum = 0;
        for (int i = 0; i < N; i++) {
            sum += step(outer, i) + outer.applyAsInt(i);
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
                int arg = (i > 3000 && i % 5000 == 0) ? 999 : i;
                sum += indirect(MAYBE_THROW, arg) + MAYBE_THROW.applyAsInt(arg);
            } catch (RuntimeException e) {
                caught++;
            }
        }
        return sum + caught * 31;
    }

    /**
     * Long enough that the background compiler certainly publishes the impl
     * body and the fast path certainly enters it.
     *
     * The other checksums here run 4 000 iterations, which crosses the tier-up
     * threshold but need not outlast an asynchronous compile — so a run in
     * which the fast path never engaged would still produce the right answer
     * and still pass. That is a test agreeing with HotSpot about a code path it
     * never took. This one exists so `lambda_jit_engagement_tests` can assert
     * the path was taken at all.
     */
    public static int warmChecksum() {
        IntUnaryOperator plus1 = v -> v + 3;
        int sum = 0;
        for (int i = 0; i < 400_000; i++) {
            sum += step(plus1, i & 0xFF) + plus1.applyAsInt(i & 0xFF);
        }
        return sum;
    }

    // The capturing fixtures below get their OWN hop methods, and that is not
    // tidiness.
    //
    // `step` is shared by every checksum in this file, so ITS call site
    // accumulates receiver classes across the whole run — three from
    // `captureShapesChecksum`, one from `multiCaptureChecksum`, one from
    // `warmCapturingChecksum`, on top of everything above. The polymorphic
    // inline cache holds four (`JIT_PIC_ENTRIES`), and the fifth receiver
    // evicts one. Past that the site thrashes and every dispatch falls back to
    // the Rust arm, which is exactly what `site_direct` is asserted to be small.
    //
    // Measured as an intermittent failure of `lambda_capture_adapter_tests` —
    // one run in eighteen — because whether the site goes megamorphic before or
    // after the bulk of the calls depends on when each body finishes compiling.
    // A dedicated hop per fixture keeps each site inside the cache's four ways,
    // and is also what an ordinary call site looks like.

    private static int capStep(IntUnaryOperator op, int v) {
        return op.applyAsInt(v);
    }

    private static int shapeStep(IntUnaryOperator op, int v) {
        return op.applyAsInt(v);
    }

    private static long shapeStepLong(LongUnaryOperator op, long v) {
        return op.applyAsLong(v);
    }

    private static double shapeStepDouble(DoubleUnaryOperator op, double v) {
        return op.applyAsDouble(v);
    }

    private static String shapeStepObj(IntFunction<String> op, int v) {
        return op.apply(v);
    }

    private static int multiStep(IntUnaryOperator op, int v) {
        return op.applyAsInt(v);
    }

    private static long multiStepLong(LongUnaryOperator op, long v) {
        return op.applyAsLong(v);
    }

    private static double multiStepDouble(DoubleUnaryOperator op, double v) {
        return op.applyAsDouble(v);
    }

    /**
     * {@link #warmChecksum} for a CAPTURING lambda.
     *
     * The non-capturing thunk is a pure register shuffle; this one has to read
     * the captured value out of the proxy object before it jumps. {@code
     * warmChecksum} cannot prove that happened — its lambda captures nothing,
     * so its site's thunk engages whatever the capture path does — which is why
     * this is a separate method with a separate engagement counter behind it.
     *
     * {@code k} is deliberately NOT {@code final}. A final local with a constant
     * initializer is a *constant variable* in the JLS sense, and javac replaces
     * every reference to one with its value before desugaring the lambda: the
     * body would capture nothing at all and this method would silently be a
     * second copy of {@code warmChecksum}.
     */
    public static int warmCapturingChecksum() {
        int k = 7;
        IntUnaryOperator capAdd = v -> v + k;
        int sum = 0;
        for (int i = 0; i < 400_000; i++) {
            sum += capStep(capAdd, i & 0xFF) + capAdd.applyAsInt(i & 0xFF);
        }
        return sum;
    }

    /**
     * One hot loop per capture WIDTH, because the thunk emits a different load
     * for each and they fail differently.
     *
     * A wide load covers a reference, a {@code long} and a {@code double}; a
     * zero-extending narrow load covers a {@code float}'s bits; a
     * sign-extending narrow load covers the whole int category. Each arm is
     * chosen so a wrong load is a wrong NUMBER rather than a crash: the
     * {@code byte} is negative (156 if read unsigned), the {@code char} is
     * above {@code 0x7FFF} (negative if read signed), the {@code long} does not
     * fit in 32 bits, and both floating-point constants are exact binary
     * fractions, so scaling them back to integers loses nothing and the
     * checksum stays exactly comparable.
     */
    public static int captureShapesChecksum() {
        long bigCap = 4_000_000_029L;
        double dCap = 0.25;
        float fCap = 0.15625f;
        byte bCap = (byte) -100;
        char cCap = (char) 0xFFFF;
        short sCap = (short) -30_000;
        String rCap = "abcd";

        LongUnaryOperator lop = v -> v + bigCap;
        DoubleUnaryOperator dop = v -> v * dCap;
        DoubleUnaryOperator fop = v -> v * fCap;
        IntUnaryOperator bop = v -> v + bCap;
        IntUnaryOperator cop = v -> v + cCap;
        IntUnaryOperator sop = v -> v + sCap;
        IntFunction<String> rop = v -> rCap + v;

        long acc = 0;
        for (int i = 0; i < N; i++) {
            int x = i & 0xFF;
            acc += shapeStepLong(lop, x);
            acc += (long) (shapeStepDouble(dop, x) * 4.0);
            acc += (long) (shapeStepDouble(fop, x) * 64.0);
            acc += shapeStep(bop, x);
            acc += shapeStep(cop, x);
            acc += shapeStep(sop, x);
            acc += shapeStepObj(rop, x).length();
        }
        return (int) (acc % 1_000_000_007L);
    }

    /**
     * Lambdas with MORE THAN ONE capture, which is the only thing that can
     * check the capture INDEX.
     *
     * {@link #captureShapesChecksum} covers one capture of each width, and
     * every one of its lambdas captures exactly one value — so a thunk that
     * ignored the capture index outright and read every capture from cell 0
     * passed it, and the engagement assertions with it. Measured, not
     * supposed: that break was applied and the suite stayed green.
     *
     * Each lambda here holds two captures and combines them
     * NON-COMMUTATIVELY, so reading both from one cell, or reading them in the
     * wrong order, changes the checksum. The first pair are the same width,
     * which isolates the index from the load; the other two mix widths, where a
     * collapsed index also picks the wrong load.
     */
    public static int multiCaptureChecksum() {
        long cl1 = 1_000_000_007L;
        long cl2 = 13L;
        int ci = 3;
        String cs = "wxyz";
        double cd = 0.5;
        int ck = 41;

        LongUnaryOperator twoLongs = v -> cl1 - cl2 * v;
        IntUnaryOperator intAndRef = v -> ci * 1000 + cs.length() + v;
        DoubleUnaryOperator dblAndInt = v -> cd * v + ck;

        long acc = 0;
        for (int i = 0; i < N; i++) {
            int x = i & 0xFF;
            acc += multiStepLong(twoLongs, x);
            acc += multiStep(intAndRef, x);
            acc += (long) (multiStepDouble(dblAndInt, x) * 2.0);
        }
        return (int) (acc % 1_000_000_007L);
    }

    private static int indirect(IntUnaryOperator op, int v) {
        return level2(op, v);
    }

    private static int level2(IntUnaryOperator op, int v) {
        return op.applyAsInt(v);
    }
}
