/**
 * Regression: an implicit NPE raised in compiled code AFTER another exception is
 * already in flight must not survive to be raised again at an unrelated call.
 *
 * The shape, all three parts of which are needed:
 *
 *   * a lambda whose body dereferences its argument, called both directly and
 *     through a one-line static hop, so the SAM call is served by the JIT-side
 *     direct arm once the caller is compiled;
 *   * a loop hot enough for back-edge OSR, so the CALLER is compiled code;
 *   * one null every 500 iterations, so a genuine NPE is raised, caught, and
 *     counted.
 *
 * When the compiled body's first trap deopts, the direct arm finishes the call
 * in the interpreter and hands back a zero with the NPE parked in
 * `jit_pending_exception`. Compiled code then evaluates the SECOND operand of
 * the same expression before its post-invoke guard fires, dereferences the same
 * null, and raises a second implicit trap that nothing will ever deliver: the
 * first exception is already unwinding. That orphaned flag used to be drained
 * by the next unrelated JIT call, which raised a NullPointerException for a
 * receiver that was never null -- one extra catch, on an iteration whose string
 * is "abc".
 *
 * `caught` is therefore the whole test: it must be exactly `N / 500`. The
 * checksum is diffed against HotSpot by the runner, so a wrong count fails even
 * without the assert.
 *
 * 200 000 iterations because the trap has to land after the caller has been
 * OSR-compiled AND after the lambda body's first deopt; at 6 000 it still
 * reproduces, and the larger count leaves room for the compile to arrive later
 * on a loaded machine.
 */
import java.util.function.Function;

public class RJitLambdaNpeSupersede {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    private static final int N = 200_000;

    /** The one-line hop that compiles at the invocation threshold. */
    private static int stepFn(Function<String, Integer> op, String v) {
        return op.apply(v);
    }

    public static void main(String[] args) {
        Function<String, Integer> lengthOf = s -> s.length();
        int sum = 0;
        int caught = 0;
        int spurious = 0;
        for (int i = 0; i < N; i++) {
            String s = (i % 500 == 499) ? null : "abc";
            try {
                sum += lengthOf.apply(s) + stepFn(lengthOf, s);
            } catch (NullPointerException e) {
                caught++;
                if (s != null) {
                    spurious++;
                }
            }
        }
        check(spurious == 0, "NullPointerException raised for a non-null receiver x" + spurious);
        check(caught == N / 500, "expected " + (N / 500) + " NPEs, got " + caught);
        check(sum == (N - N / 500) * 6, "sum " + sum);

        System.out.println("CK sum=" + sum + " caught=" + caught + " spurious=" + spurious
                + " checksum=" + (sum + caught * 31));
        System.out.println("PASS RJitLambdaNpeSupersede (" + checks + " checks)");
    }
}
