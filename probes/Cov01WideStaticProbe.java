/**
 * cov-01 residual — do `long`, `double` and `float` `getstatic` reads produce
 * the right value on the OPTIMIZING (C2/IR) tier?
 *
 * <p>Three things make this worth a probe rather than only unit tests:
 *
 * <ul>
 *   <li>{@link #readMinPlusOne} is the case the widths exist to get wrong.
 *       {@code jit_getstatic} reports a failed {@code <clinit>} by returning
 *       {@code i64::MIN}, and {@code Long.MIN_VALUE} is bit-identical to it, so
 *       a read of such a static must not be mistaken for an exception. The
 *       {@code + 1} is what makes the two answers different bit patterns —
 *       without it, "kept the value" and "propagated the sentinel" both produce
 *       {@code Long.MIN_VALUE} and the check cannot fail.
 *   <li>{@link #passWideToCall} is the shape the admission gate cannot see.
 *       {@code getstatic} is listed by neither {@code is_category2_opcode} nor
 *       {@code is_float_opcode}, so this method — which contains no other
 *       category-2 or floating-point opcode — is admitted to the optimizing
 *       pipeline through the INT clause while carrying a `long`.
 *   <li>Every static here is deliberately NON-final. A {@code static final}
 *       primitive with a constant initializer is a compile-time constant: javac
 *       emits {@code ldc2_w}, not {@code getstatic}, so a `final` field would
 *       test the wrong opcode. That is also why wide static READS are rare in
 *       real code, and why this lane's measured population is near zero.
 * </ul>
 *
 * <p>Run under {@code CRATONVM_DBG=ir-compiles} and confirm
 * {@code [ir] optimizing backend produced a body} names these readers; a rung
 * that never reached the optimizing tier is testing the single-pass backend,
 * which has compiled all of these for months.
 *
 * <p>Usage: {@code Cov01WideStaticProbe [iterations]}
 */
public final class Cov01WideStaticProbe {

    static long wideL = 0x0123_4567_89AB_CDEFL;
    static long wideMin = Long.MIN_VALUE;
    static double wideD = -2.5d;
    static float wideF = -2.5f;
    static int narrowI = -7;

    static long sink;

    static long readL() {
        return wideL;
    }

    /**
     * The sentinel-collision case. `+ 1` is load-bearing: see the class doc.
     */
    static long readMinPlusOne() {
        return wideMin + 1;
    }

    static double readD() {
        return wideD;
    }

    static float readF() {
        return wideF;
    }

    /** The non-reference control, so a total failure is distinguishable. */
    static int readI() {
        return narrowI;
    }

    static void accept(long v) {
        sink += v;
    }

    /**
     * `getstatic <J>; invokestatic accept(J)V; return` — a `long` in a method
     * with no category-2 opcode, so the admission gate sees an int-only method.
     */
    static void passWideToCall() {
        accept(wideL);
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300000;

        long expectedL = 0x0123_4567_89AB_CDEFL;
        long expectedMinPlusOne = Long.MIN_VALUE + 1;
        long checked = 0;

        for (int i = 0; i < iters; i++) {
            long l = readL();
            if (l != expectedL) {
                throw new AssertionError("readL = " + Long.toHexString(l) + " at i=" + i);
            }
            long m = readMinPlusOne();
            if (m != expectedMinPlusOne) {
                throw new AssertionError(
                        "readMinPlusOne = "
                                + m
                                + " at i="
                                + i
                                + " — Long.MIN_VALUE was mistaken for the failed-<clinit>"
                                + " sentinel");
            }
            double d = readD();
            if (Double.doubleToRawLongBits(d) != Double.doubleToRawLongBits(-2.5d)) {
                throw new AssertionError("readD = " + d + " at i=" + i);
            }
            float f = readF();
            if (Float.floatToRawIntBits(f) != Float.floatToRawIntBits(-2.5f)) {
                throw new AssertionError("readF = " + f + " at i=" + i);
            }
            if (readI() != -7) {
                throw new AssertionError("readI control failed at i=" + i);
            }
            passWideToCall();
            checked++;
        }

        long expectedSink = expectedL * iters;
        if (sink != expectedSink) {
            throw new AssertionError(
                    "passWideToCall accumulated " + sink + ", expected " + expectedSink);
        }
        System.out.println(
                "Cov01WideStaticProbe OK iters=" + iters + " checked=" + checked + " sink=" + sink);
    }
}
