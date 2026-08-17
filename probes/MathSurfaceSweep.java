import java.util.function.*;

/**
 * Exhaustive differential sweep over every java.lang.Math / java.lang.StrictMath
 * method CratonVM backs with a native. Every result is printed as RAW BITS so
 * -0.0, NaN payloads and subnormals compare exactly; run under HotSpot and under
 * CratonVM and diff the two outputs line for line. A clean diff is the ratchet:
 * any new row is a divergence from the oracle.
 *
 * Two-argument rows where BOTH arguments are NaN are skipped on purpose. The
 * JLS does not say which NaN such a call returns, and on x86 the answer is
 * decided by which operand the register allocator put in the destination of the
 * add — HotSpot's own answer there is a JIT artifact, not a contract, so
 * pinning it would make this instrument fail for the wrong reason.
 */
public class MathSurfaceSweep {
    static final StringBuilder OUT = new StringBuilder();

    static final double[] D = {
        0.0, -0.0, 1.0, -1.0, 0.5, -0.5, 2.0, 3.0, -3.0, 0.1, -0.1,
        Math.PI, Math.E, 100.0, -100.0, 1e300, -1e300, 1e-300, -1e-300,
        Double.MAX_VALUE, -Double.MAX_VALUE, Double.MIN_VALUE, -Double.MIN_VALUE,
        Double.MIN_NORMAL, -Double.MIN_NORMAL,
        Math.nextDown(Double.MIN_NORMAL), -Math.nextDown(Double.MIN_NORMAL),
        Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY, Double.NaN,
        Double.longBitsToDouble(0xFFF8AE0A00000000L),
        4.5, -4.5, 5.5, -5.5, 2.5, -2.5, 0.49999999999999994, -0.49999999999999994,
        4503599627370495.5, 9.007199254740992E15, 1.5, -1.5
    };
    static final float[] F = {
        0.0f, -0.0f, 1.0f, -1.0f, 0.5f, -0.5f, 3.0f, -3.0f,
        Float.MAX_VALUE, -Float.MAX_VALUE, Float.MIN_VALUE, -Float.MIN_VALUE,
        Float.MIN_NORMAL, -Float.MIN_NORMAL,
        Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY, Float.NaN,
        Float.intBitsToFloat(0xFFC8AE0A),
        2.5f, -2.5f, 4.5f, -4.5f, 0.49999997f, 8388607.5f
    };
    static final int[] I = {
        Integer.MIN_VALUE, Integer.MIN_VALUE + 1, -65536, -7, -2, -1, 0, 1, 2, 7,
        65536, Integer.MAX_VALUE - 1, Integer.MAX_VALUE
    };
    static final long[] L = {
        Long.MIN_VALUE, Long.MIN_VALUE + 1, -4294967296L, -7L, -2L, -1L, 0L, 1L, 2L, 7L,
        4294967296L, Long.MAX_VALUE - 1, Long.MAX_VALUE
    };

    static void p(String s) { OUT.append(s).append(System.lineSeparator()); }
    static String b(double d) { return Long.toHexString(Double.doubleToRawLongBits(d)); }
    static String b(float f)  { return Integer.toHexString(Float.floatToRawIntBits(f)); }

    static void d1(String name, DoubleUnaryOperator f) {
        for (double x : D) {
            String r;
            try { r = b(f.applyAsDouble(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + b(x) + ") = " + r);
        }
    }
    static void d2(String name, DoubleBinaryOperator f) {
        for (double x : D) for (double y : D) {
            if (Double.isNaN(x) && Double.isNaN(y)) continue; // unspecified, see header
            String r;
            try { r = b(f.applyAsDouble(x, y)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + b(x) + "," + b(y) + ") = " + r);
        }
    }
    interface FF { float apply(float a); }
    interface FF2 { float apply(float a, float b); }
    static void f1(String name, FF f) {
        for (float x : F) {
            String r;
            try { r = b(f.apply(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + b(x) + ") = " + r);
        }
    }
    static void f2(String name, FF2 f) {
        for (float x : F) for (float y : F) {
            if (Float.isNaN(x) && Float.isNaN(y)) continue; // unspecified, see header
            String r;
            try { r = b(f.apply(x, y)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + b(x) + "," + b(y) + ") = " + r);
        }
    }
    interface DI { int apply(double a); }
    static void di(String name, DI f) {
        for (double x : D) {
            String r;
            try { r = Integer.toString(f.apply(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + b(x) + ") = " + r);
        }
    }
    interface DL { long apply(double a); }
    static void dl(String name, DL f) {
        for (double x : D) {
            String r;
            try { r = Long.toString(f.apply(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + b(x) + ") = " + r);
        }
    }
    interface FI { int apply(float a); }
    static void fi(String name, FI f) {
        for (float x : F) {
            String r;
            try { r = Integer.toString(f.apply(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + b(x) + ") = " + r);
        }
    }
    static void i1(String name, IntUnaryOperator f) {
        for (int x : I) {
            String r;
            try { r = Integer.toString(f.applyAsInt(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + x + ") = " + r);
        }
    }
    static void i2(String name, IntBinaryOperator f) {
        for (int x : I) for (int y : I) {
            String r;
            try { r = Integer.toString(f.applyAsInt(x, y)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + x + "," + y + ") = " + r);
        }
    }
    static void l1(String name, LongUnaryOperator f) {
        for (long x : L) {
            String r;
            try { r = Long.toString(f.applyAsLong(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + x + ") = " + r);
        }
    }
    static void l2(String name, LongBinaryOperator f) {
        for (long x : L) for (long y : L) {
            String r;
            try { r = Long.toString(f.applyAsLong(x, y)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + x + "," + y + ") = " + r);
        }
    }
    interface LI { int apply(long a); }
    static void li(String name, LI f) {
        for (long x : L) {
            String r;
            try { r = Integer.toString(f.apply(x)); } catch (Throwable t) { r = "EXC:" + t.getClass().getName(); }
            p(name + "(" + x + ") = " + r);
        }
    }

    public static void main(String[] a) {
        d1("M.abs", Math::abs);          d1("S.abs", StrictMath::abs);
        d1("M.acos", Math::acos);        d1("S.acos", StrictMath::acos);
        d1("M.asin", Math::asin);        d1("S.asin", StrictMath::asin);
        d1("M.atan", Math::atan);        d1("S.atan", StrictMath::atan);
        d1("M.cbrt", Math::cbrt);        d1("S.cbrt", StrictMath::cbrt);
        d1("M.ceil", Math::ceil);        d1("S.ceil", StrictMath::ceil);
        d1("M.cos", Math::cos);          d1("S.cos", StrictMath::cos);
        d1("M.cosh", Math::cosh);        d1("S.cosh", StrictMath::cosh);
        d1("M.exp", Math::exp);          d1("S.exp", StrictMath::exp);
        d1("M.expm1", Math::expm1);      d1("S.expm1", StrictMath::expm1);
        d1("M.floor", Math::floor);      d1("S.floor", StrictMath::floor);
        d1("M.log", Math::log);          d1("S.log", StrictMath::log);
        d1("M.log10", Math::log10);      d1("S.log10", StrictMath::log10);
        d1("M.log1p", Math::log1p);      d1("S.log1p", StrictMath::log1p);
        d1("M.rint", Math::rint);        d1("S.rint", StrictMath::rint);
        d1("M.signum", Math::signum);    d1("S.signum", StrictMath::signum);
        d1("M.sin", Math::sin);          d1("S.sin", StrictMath::sin);
        d1("M.sinh", Math::sinh);        d1("S.sinh", StrictMath::sinh);
        d1("M.sqrt", Math::sqrt);        d1("S.sqrt", StrictMath::sqrt);
        d1("M.tan", Math::tan);          d1("S.tan", StrictMath::tan);
        d1("M.tanh", Math::tanh);        d1("S.tanh", StrictMath::tanh);
        d1("M.toDegrees", Math::toDegrees); d1("S.toDegrees", StrictMath::toDegrees);
        d1("M.toRadians", Math::toRadians); d1("S.toRadians", StrictMath::toRadians);
        d1("M.ulp", Math::ulp);          d1("S.ulp", StrictMath::ulp);
        d1("M.nextUp", Math::nextUp);    d1("S.nextUp", StrictMath::nextUp);
        d1("M.nextDown", Math::nextDown); d1("S.nextDown", StrictMath::nextDown);
        di("M.getExponent", Math::getExponent);
        di("S.getExponent", StrictMath::getExponent);
        dl("M.round", Math::round);      dl("S.round", StrictMath::round);

        d2("M.atan2", Math::atan2);      d2("S.atan2", StrictMath::atan2);
        d2("M.copySign", Math::copySign); d2("S.copySign", StrictMath::copySign);
        d2("M.hypot", Math::hypot);      d2("S.hypot", StrictMath::hypot);
        d2("M.IEEEremainder", Math::IEEEremainder);
        d2("S.IEEEremainder", StrictMath::IEEEremainder);
        d2("M.max", Math::max);          d2("S.max", StrictMath::max);
        d2("M.min", Math::min);          d2("S.min", StrictMath::min);
        d2("M.pow", Math::pow);          d2("S.pow", StrictMath::pow);
        d2("M.nextAfter", Math::nextAfter); d2("S.nextAfter", StrictMath::nextAfter);

        f1("M.absF", Math::abs);         f1("S.absF", StrictMath::abs);
        f1("M.signumF", Math::signum);   f1("S.signumF", StrictMath::signum);
        f1("M.ulpF", Math::ulp);         f1("S.ulpF", StrictMath::ulp);
        f2("M.copySignF", Math::copySign); f2("S.copySignF", StrictMath::copySign);
        f2("M.maxF", Math::max);         f2("S.maxF", StrictMath::max);
        f2("M.minF", Math::min);         f2("S.minF", StrictMath::min);
        fi("M.roundF", Math::round);     fi("S.roundF", StrictMath::round);

        i1("M.absI", Math::abs);         i1("S.absI", StrictMath::abs);
        i1("M.negateExactI", Math::negateExact); i1("S.negateExactI", StrictMath::negateExact);
        i1("M.incrementExactI", Math::incrementExact); i1("S.incrementExactI", StrictMath::incrementExact);
        i1("M.decrementExactI", Math::decrementExact); i1("S.decrementExactI", StrictMath::decrementExact);
        i2("M.maxI", Math::max);         i2("S.maxI", StrictMath::max);
        i2("M.minI", Math::min);         i2("S.minI", StrictMath::min);
        i2("M.addExactI", Math::addExact); i2("S.addExactI", StrictMath::addExact);
        i2("M.subtractExactI", Math::subtractExact); i2("S.subtractExactI", StrictMath::subtractExact);
        i2("M.multiplyExactI", Math::multiplyExact); i2("S.multiplyExactI", StrictMath::multiplyExact);
        i2("M.floorDivI", Math::floorDiv); i2("S.floorDivI", StrictMath::floorDiv);
        i2("M.floorModI", Math::floorMod); i2("S.floorModI", StrictMath::floorMod);

        l1("M.absJ", Math::abs);         l1("S.absJ", StrictMath::abs);
        l1("M.negateExactJ", Math::negateExact); l1("S.negateExactJ", StrictMath::negateExact);
        l1("M.incrementExactJ", Math::incrementExact); l1("S.incrementExactJ", StrictMath::incrementExact);
        l1("M.decrementExactJ", Math::decrementExact); l1("S.decrementExactJ", StrictMath::decrementExact);
        li("M.toIntExact", Math::toIntExact); li("S.toIntExact", StrictMath::toIntExact);
        l2("M.maxJ", Math::max);         l2("S.maxJ", StrictMath::max);
        l2("M.minJ", Math::min);         l2("S.minJ", StrictMath::min);
        l2("M.addExactJ", Math::addExact); l2("S.addExactJ", StrictMath::addExact);
        l2("M.subtractExactJ", Math::subtractExact); l2("S.subtractExactJ", StrictMath::subtractExact);
        l2("M.multiplyExactJ", Math::multiplyExact); l2("S.multiplyExactJ", StrictMath::multiplyExact);
        l2("M.floorDivJ", Math::floorDiv); l2("S.floorDivJ", StrictMath::floorDiv);
        l2("M.floorModJ", Math::floorMod); l2("S.floorModJ", StrictMath::floorMod);
        l2("M.multiplyHigh", Math::multiplyHigh); l2("S.multiplyHigh", StrictMath::multiplyHigh);
        l2("M.unsignedMultiplyHigh", Math::unsignedMultiplyHigh);
        l2("S.unsignedMultiplyHigh", StrictMath::unsignedMultiplyHigh);

        System.out.print(OUT);
        System.out.println("SWEEP_END");
    }
}
