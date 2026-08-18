import java.util.function.*;

/**
 * The shapes a hand-emitted inline-cache thunk has to get right.
 *
 * The thunk drops the lambda proxy receiver and slides every SAM argument down
 * one register before tail-jumping to the implementation. That is three things
 * a register shuffle can get wrong and nothing else can:
 *
 *   - the WIDTH of what it moves (an `int` and a `long` share a register, a
 *     `double` does not — an FP argument never travels in the integer registers
 *     the thunk shuffles, so a double-taking SAM must be refused, not mangled);
 *   - the ORDER it moves them in (slide two arguments in the wrong direction
 *     and the second one arrives holding the first one's value);
 *   - the ARITY it will accept (the receiver and the hidden context register
 *     both consume argument registers, so a SAM that fits the incoming side may
 *     not fit the outgoing one).
 *
 * Every arm loops far past the tier-up threshold so the impl is compiled and
 * the thunk is installed, then keeps going so the great majority of the calls
 * go through the inline cache rather than Rust. Read the output against
 * HotSpot's — the numbers are ordinary arithmetic, and any disagreement is the
 * shuffle.
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

    /**
     * Each arm is its OWN method, and that is not tidiness.
     *
     * With every loop in `main`, the frame is far too large for the
     * compiler and every SAM call is made from the interpreter — measured,
     * `site_calls=0` across 3 400 000 dispatches, so the probe exercised
     * the interpreted path and said nothing whatsoever about the thunk it
     * was written to test. A small method with a tight loop is compiled on
     * its back edges, and only then does the call site this probe is about
     * exist at all.
     */
    public static void main(String[] args) {
        arm1();
        arm2();
        arm3();
        arm4();
        arm5();
        arm6();
        arm7();
        arm8();
        arm9();
        arm10();
        arm11();
        arm12();
        System.out.println("ALL-DONE");
    }

    private static void arm1() {
        // 1 — one argument. The minimal shuffle: `mov ARG0, ARG1`.
        IntUnaryOperator one = v -> v * 3 + 1;
        long s1 = 0;
        for (int i = 0; i < N; i++) s1 += one.applyAsInt(i & 0xFFFF);
        System.out.println("1 one_arg=" + s1);
    }

    private static void arm2() {
        // 2 — TWO arguments, NON-commutative on purpose. A shuffle that slides
        // in the wrong order yields a - a == 0, or b - b, not a - b.
        Int2 sub = (a, b) -> a - b;
        long s2 = 0;
        for (int i = 0; i < N; i++) s2 += sub.apply(i, i / 3);
        System.out.println("2 two_args=" + s2);
    }

    private static void arm3() {
        // 3 — THREE arguments, each weighted differently, so any permutation
        // shows up in the sum.
        Int3 weigh = (a, b, c) -> a + 10 * b + 100 * c;
        long s3 = 0;
        for (int i = 0; i < N; i++) s3 += weigh.apply(i & 7, i & 3, i & 1);
        System.out.println("3 three_args=" + s3);
    }

    private static void arm4() {
        // 4 — a method reference to a static, which is the same impl shape a
        // non-capturing lambda compiles to but reached through a different
        // bootstrap.
        Int2 mref = LambdaAdapterProbe::subtract;
        long s4 = 0;
        for (int i = 0; i < N; i++) s4 += mref.apply(i, 1);
        System.out.println("4 mref_two_args=" + s4);
    }

    private static void arm5() {
        // 5 — a `long` argument and a `long` return: full 64-bit width through
        // the same registers. A 32-bit move would truncate this.
        LongOp big = v -> v * 2_147_483_647L;
        long s5 = 0;
        for (int i = 0; i < N; i++) s5 += big.apply(i & 0xFFFF) & 0xFFFF_FFFFL;
        System.out.println("5 long_arg=" + s5);
    }

    private static void arm6() {
        // 6 — mixed int/long widths in one call.
        MixedOp mixed = (a, b) -> a + b * 1_000_000_007L;
        long s6 = 0;
        for (int i = 0; i < N; i++) s6 += mixed.apply(i & 0xFF, i & 0xFFFF) & 0xFFFF_FFFFL;
        System.out.println("6 mixed_widths=" + s6);
    }

    private static void arm7() {
        // 7 — REFERENCE arguments and a reference return: the registers hold
        // object pointers, and a mixed-up pair is a wrong string, not a crash.
        ObjOp cat = (a, b) -> a + "/" + b;
        int s7 = 0;
        for (int i = 0; i < N; i++) s7 += cat.apply("a" + (i & 3), "b" + (i & 7)).length();
        System.out.println("7 obj_args=" + s7);
    }

    private static void arm8() {
        // 8 — VOID return. The thunk must not fabricate a value.
        final int[] sink = new int[1];
        VoidOp v = x -> sink[0] += x;
        for (int i = 0; i < N; i++) v.run(i & 0xFF);
        System.out.println("8 void=" + sink[0]);
    }

    private static void arm9() {
        // 9 — ZERO arguments: the thunk is a bare tail jump, the receiver
        // simply not passed on.
        NoArg none = () -> 7;
        long s9 = 0;
        for (int i = 0; i < N; i++) s9 += none.get();
        System.out.println("9 no_args=" + s9);
    }

    private static void arm10() {
        // 10 — a DOUBLE argument, which travels in an FP register the thunk
        // never touches. This is the arm that catches a thunk installed for a
        // shape it cannot serve.
        DoubleOp dop = x -> x / 3.0;
        double s10 = 0;
        for (int i = 0; i < N; i++) s10 += dop.apply(i & 0xFF);
        System.out.printf("10 double_arg=%.6f%n", s10);
    }

    private static void arm11() {
        // 11 — the SAME functional interface with two different lambdas at one
        // call site, so the site goes polymorphic and the cache must not answer
        // the second with the first's thunk.
        IntUnaryOperator plus = x -> x + 5;
        IntUnaryOperator times = x -> x * 5;
        long s11 = 0;
        for (int i = 0; i < N; i++) {
            IntUnaryOperator op = (i & 1) == 0 ? plus : times;
            s11 += op.applyAsInt(i & 0xFF);
        }
        System.out.println("11 polymorphic=" + s11);
    }

    private static void arm12() {
        // 12 — an exception out of a warm, adapter-dispatched body: it must
        // still reach the handler at the call site.
        IntUnaryOperator boom = x -> {
            if (x == 4242) throw new IllegalStateException("boom");
            return x + 1;
        };
        int caught = 0;
        long s12 = 0;
        for (int i = 0; i < N; i++) {
            int arg = (i > 1000 && i % 7000 == 0) ? 4242 : (i & 0xFF);
            try {
                s12 += boom.applyAsInt(arg);
            } catch (IllegalStateException e) {
                caught++;
            }
        }
        System.out.println("12 throwing=" + s12 + " caught=" + caught);
    }
}
