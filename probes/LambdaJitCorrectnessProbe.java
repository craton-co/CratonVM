import java.util.function.*;
import java.util.*;

/**
 * Correctness gate for the lambda SAM JIT tier-up fast path
 * (CRATONVM_JIT_LAMBDA_TIERUP, vm/src/runtime/interpreter/lambda.rs +
 * jit_bridge.rs's execute_jit_call_oneshot).
 *
 * Every arm below is a path that ONLY the fast path can get wrong, and every
 * one runs enough iterations to get its lambda body nominated, compiled, and
 * then re-entered through the compiled entry — the happy-path-only probes that
 * preceded this one could not have caught any of them.
 *
 * The transcript is deterministic and is meant to be DIFFED against HotSpot's,
 * not read for plausibility: `java LambdaJitCorrectnessProbe > hotspot.txt`
 * versus `cratonvm LambdaJitCorrectnessProbe > craton.txt`.
 *
 *   1  int  -> int, non-capturing            return-value conversion, b'I'
 *   2  capturing                             capture prepending + fast path
 *   3  method reference (InvokeStatic impl)
 *   4  THROWS after warmup                   the section 5.3 crash repro:
 *                                            a cold `throw` branch compiles to
 *                                            an uncommon trap, so the first
 *                                            throwing call DEOPTS
 *   5  default method through a lambda
 *   6  bound instance method reference
 *   7  long / double / Object / void returns  every arm of the one-shot's
 *                                             return-type conversion
 *   8  ArithmeticException from the body      the `sig.arithmetic` drain
 *   9  NullPointerException from the body     the `sig.npe` drain
 *  10  ArrayIndexOutOfBounds from the body    the `sig.aioobe` drain
 *  11  lambda body with its OWN try/catch     the exception-table gate: the
 *                                             fast path must DECLINE and the
 *                                             interpreted path must still be
 *                                             correct
 *  12  nested lambda calling a lambda         re-entrant dispatch under the
 *                                             fast path
 *  13  throw straight through an uncaught
 *      frame into an outer handler            propagation, not local routing
 */
public class LambdaJitCorrectnessProbe {

    private static final int N = 4000;

    private static int addOne(int x) { return x + 1; }

    private static final IntUnaryOperator maybeThrow = v -> {
        if (v == 999) throw new RuntimeException("boom-" + v);
        return v * 2;
    };

    private static final IntUnaryOperator divider = v -> 1000 / (v % 500);

    private static final Function<String, Integer> lengthOf = s -> s.length();

    private static final IntUnaryOperator indexer = v -> {
        int[] a = new int[4];
        return a[v % 8];
    };

    // Body carries its own exception table — the fast path must decline it.
    private static final IntUnaryOperator selfCatching = v -> {
        try {
            if (v % 7 == 0) throw new IllegalStateException("x");
            return v;
        } catch (IllegalStateException e) {
            return -1;
        }
    };

    private static Function<Integer, Integer> capturing(int k) {
        return v -> v + k;
    }

    public static void main(String[] args) throws Exception {
        // 1
        IntUnaryOperator plus1 = v -> v + 1;
        long sum = 0;
        for (int i = 0; i < N; i++) sum += plus1.applyAsInt(i);
        System.out.println("1 plain=" + sum);

        // 2
        Function<Integer, Integer> capAdd = capturing(7);
        long sum2 = 0;
        for (int i = 0; i < N; i++) sum2 += capAdd.apply(i);
        System.out.println("2 capturing=" + sum2);

        // 3
        IntUnaryOperator mref = LambdaJitCorrectnessProbe::addOne;
        long sum3 = 0;
        for (int i = 0; i < N; i++) sum3 += mref.applyAsInt(i);
        System.out.println("3 mref=" + sum3);

        // 4 — the crash repro. The throwing input arrives LATE, after the body
        // is compiled, and then repeatedly, so a corrupted frame/stack state
        // left by the first throw shows up on the calls that follow it.
        int caught = 0;
        long sum4 = 0;
        StringBuilder msgs = new StringBuilder();
        for (int i = 0; i < N; i++) {
            int v = (i > 2000 && i % 500 == 0) ? 999 : i;
            try {
                sum4 += maybeThrow.applyAsInt(v);
            } catch (RuntimeException e) {
                caught++;
                msgs.append(e.getMessage()).append(';');
            }
        }
        System.out.println("4 throwing=" + sum4 + " caught=" + caught + " msgs=" + msgs);

        // 5
        IntUnaryOperator base = v -> v + 100;
        IntUnaryOperator composed = base.andThen(v -> v * 2);
        long sum5 = 0;
        for (int i = 0; i < N; i++) sum5 += composed.applyAsInt(i);
        System.out.println("5 composed=" + sum5);

        // 6
        StringBuilder sb = new StringBuilder();
        Consumer<String> collector = sb::append;
        for (int i = 0; i < 1000; i++) collector.accept("n" + (i % 10));
        System.out.println("6 bound_mref_len=" + sb.length());

        // 7 — every return-type arm of the one-shot conversion.
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
        final int[] sink = new int[1];
        IntConsumer voidOp = v -> sink[0] += v;
        for (int i = 0; i < N; i++) voidOp.accept(i % 5);
        System.out.println("7 long=" + lsum + " double=" + dsum + " obj=" + ssum + " void=" + sink[0]);

        // 8 — ArithmeticException raised INSIDE the (compiled) body.
        int arith = 0;
        long asum = 0;
        for (int i = 0; i < N; i++) {
            try {
                asum += divider.applyAsInt(i + 1);
            } catch (ArithmeticException e) {
                arith++;
            }
        }
        System.out.println("8 arith_sum=" + asum + " caught=" + arith);

        // 9 — NPE raised INSIDE the body.
        int npes = 0;
        long nsum = 0;
        for (int i = 0; i < N; i++) {
            String s = (i % 700 == 0) ? null : "abc";
            try {
                nsum += lengthOf.apply(s);
            } catch (NullPointerException e) {
                npes++;
            }
        }
        System.out.println("9 npe_sum=" + nsum + " caught=" + npes);

        // 10 — AIOOBE raised INSIDE the body.
        int aioobes = 0;
        long isum = 0;
        for (int i = 0; i < N; i++) {
            try {
                isum += indexer.applyAsInt(i);
            } catch (ArrayIndexOutOfBoundsException e) {
                aioobes++;
            }
        }
        System.out.println("10 aioobe_sum=" + isum + " caught=" + aioobes);

        // 11 — body with its own handler.
        long csum = 0;
        for (int i = 0; i < N; i++) csum += selfCatching.applyAsInt(i);
        System.out.println("11 self_catching=" + csum);

        // 12 — a lambda whose body calls another lambda.
        IntUnaryOperator inner = v -> v * 2;
        IntUnaryOperator outer = v -> inner.applyAsInt(v) + 1;
        long osum = 0;
        for (int i = 0; i < N; i++) osum += outer.applyAsInt(i);
        System.out.println("12 nested=" + osum);

        // 13 — an exception thrown in a warm lambda body and caught two Java
        // frames further out, so nothing local can route it.
        int outerCaught = 0;
        for (int i = 0; i < N; i++) {
            try {
                indirect(maybeThrow, (i > 3000 && i % 300 == 0) ? 999 : i);
            } catch (RuntimeException e) {
                outerCaught++;
            }
        }
        System.out.println("13 outer_caught=" + outerCaught);

        System.out.println("ALL-DONE");
    }

    private static int indirect(IntUnaryOperator op, int v) {
        return level2(op, v);
    }

    private static int level2(IntUnaryOperator op, int v) {
        return op.applyAsInt(v);
    }
}
