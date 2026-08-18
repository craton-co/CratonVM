import java.util.function.*;

/**
 * The shapes a hand-emitted inline-cache thunk has to get right.
 *
 * The thunk drops the lambda proxy receiver and slides every SAM argument down
 * one register before tail-jumping to the implementation. That is three things
 * a register shuffle can get wrong and nothing else can:
 *
 *   - the WIDTH of what it moves (an `int` and a `long` share a register; a
 *     `double` does not travel in the integer registers the thunk shuffles at
 *     all, so a double-taking SAM must be REFUSED rather than mangled);
 *   - the ORDER it moves them in (slide two arguments the wrong way and the
 *     second arrives holding the first one's value);
 *   - the ARITY it accepts (the receiver, and the hidden context register when
 *     the impl wants one, both consume argument registers).
 *
 * <h2>Why every loop is a separate one-line method</h2>
 *
 * A thunk only exists once the CALLING frame is compiled, and a frame is
 * compiled only if the compiler will take it. With all twelve loops in `main`,
 * measured, the census read {@code site_calls=0 site_adapters=0} across
 * 3 400 000 dispatches: the probe ran entirely in the interpreter and its
 * agreement with HotSpot said nothing whatsoever about the thunk it was written
 * to test. Splitting one loop per method was not enough either. What works is
 * the shape below — a tight loop over a SAM taken as a PARAMETER, with the
 * printing left to the caller — which is small enough to be compiled on its
 * back edges. Read {@code CRATONVM_DBG=lambda-jit}'s {@code site_adapters}
 * before believing any run of this file.
 */
public class LambdaAdapterProbe {

    private static final int N = 300_000;

    interface Int2 { int apply(int a, int b); }
    interface Int3 { int apply(int a, int b, int c); }
    interface LongOp { long apply(long a); }
    interface MixedOp { long apply(int a, long b); }
    interface ObjOp { String apply(String a, String b); }
    interface VoidOp { void run(int a); }
    interface NoArg { int get(); }
    interface DoubleOp { double apply(double a); }

    private static int subtract(int a, int b) { return a - b; }

    // ---- the loops: one shape each, nothing else in the frame ----

    private static long loop1(IntUnaryOperator op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.applyAsInt(i & 0xFFFF);
        return s;
    }

    private static long loop2(Int2 op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i, i / 3);
        return s;
    }

    private static long loop3(Int3 op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i & 7, i & 3, i & 1);
        return s;
    }

    private static long loop4(Int2 op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i, 1);
        return s;
    }

    private static long loop5(LongOp op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i & 0xFFFF) & 0xFFFF_FFFFL;
        return s;
    }

    private static long loop6(MixedOp op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i & 0xFF, i & 0xFFFF) & 0xFFFF_FFFFL;
        return s;
    }

    private static long loop7(ObjOp op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.apply("a" + (i & 3), "b" + (i & 7)).length();
        return s;
    }

    private static void loop8(VoidOp op) {
        for (int i = 0; i < N; i++) op.run(i & 0xFF);
    }

    private static long loop9(NoArg op) {
        long s = 0;
        for (int i = 0; i < N; i++) s += op.get();
        return s;
    }

    private static double loop10(DoubleOp op) {
        double s = 0;
        for (int i = 0; i < N; i++) s += op.apply(i & 0xFF);
        return s;
    }

    private static long loop11(IntUnaryOperator a, IntUnaryOperator b) {
        long s = 0;
        for (int i = 0; i < N; i++) s += ((i & 1) == 0 ? a : b).applyAsInt(i & 0xFF);
        return s;
    }

    private static long loop12(IntUnaryOperator op, int[] caught) {
        long s = 0;
        for (int i = 0; i < N; i++) {
            int arg = (i > 1000 && i % 7000 == 0) ? 4242 : (i & 0xFF);
            try {
                s += op.applyAsInt(arg);
            } catch (IllegalStateException e) {
                caught[0]++;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        // 1 — one argument. The minimal shuffle: `mov ARG0, ARG1`.
        System.out.println("1 one_arg=" + loop1(v -> v * 3 + 1));

        // 2 — TWO arguments, NON-commutative on purpose. A shuffle that slides
        // the wrong way yields a - a, or b - b, never a - b.
        System.out.println("2 two_args=" + loop2((a, b) -> a - b));

        // 3 — THREE arguments, each weighted differently, so any permutation
        // shows up in the sum.
        System.out.println("3 three_args=" + loop3((a, b, c) -> a + 10 * b + 100 * c));

        // 4 — a method reference to a static: the same impl shape a
        // non-capturing lambda compiles to, through a different bootstrap.
        System.out.println("4 mref_two_args=" + loop4(LambdaAdapterProbe::subtract));

        // 5 — `long` in and out: full 64-bit width. A 32-bit move truncates it.
        System.out.println("5 long_arg=" + loop5(v -> v * 2_147_483_647L));

        // 6 — mixed int/long widths in one call.
        System.out.println("6 mixed_widths=" + loop6((a, b) -> a + b * 1_000_000_007L));

        // 7 — REFERENCE arguments and return: the registers hold object
        // pointers, and a mixed-up pair is a wrong string, not a crash.
        System.out.println("7 obj_args=" + loop7((a, b) -> a + "/" + b));

        // 8 — VOID return. The thunk must not fabricate a value.
        final int[] sink = new int[1];
        loop8(x -> sink[0] += x);
        System.out.println("8 void=" + sink[0]);

        // 9 — ZERO arguments: a bare tail jump, the receiver simply not passed.
        System.out.println("9 no_args=" + loop9(() -> 7));

        // 10 — a DOUBLE argument, which travels in an FP register the thunk
        // never touches. The arm that catches a thunk installed for a shape it
        // cannot serve.
        System.out.printf("10 double_arg=%.6f%n", loop10(x -> x / 3.0));

        // 11 — two different lambdas at ONE call site, so it goes polymorphic
        // and the cache must not answer the second with the first's thunk.
        System.out.println("11 polymorphic=" + loop11(x -> x + 5, x -> x * 5));

        // 12 — an exception out of a warm, thunk-dispatched body must still
        // reach the handler at the call site.
        final int[] caught = new int[1];
        long s12 = loop12(x -> {
            if (x == 4242) throw new IllegalStateException("boom");
            return x + 1;
        }, caught);
        System.out.println("12 throwing=" + s12 + " caught=" + caught[0]);

        System.out.println("ALL-DONE");
    }
}
