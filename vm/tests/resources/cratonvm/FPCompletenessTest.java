// Declares the package the corpus invokes it under (`cratonvm/
// FPCompletenessTest`); see the note in PgoTest.java for why its absence made
// all 22 of these tests fail with ClassNotFound once the fixtures were being
// compiled rather than read from the committed .class beside this file.
package cratonvm;

public class FPCompletenessTest {

    // --- NaN / overflow conversions ---

    public static int testD2iNaN() {
        double nan = Double.NaN;
        return (int) nan; // JVM spec: 0
    }

    public static int testD2iPosOverflow() {
        double big = 3.0e10;
        return (int) big; // > Integer.MAX_VALUE → 2147483647
    }

    public static int testD2iNegOverflow() {
        double big = -3.0e10;
        return (int) big; // < Integer.MIN_VALUE → -2147483648
    }

    public static long testD2lNaN() {
        double nan = Double.NaN;
        return (long) nan; // 0L
    }

    public static long testD2lPosOverflow() {
        double big = 1.0e19;
        return (long) big; // > Long.MAX_VALUE → 9223372036854775807
    }

    public static long testD2lNegOverflow() {
        double big = -1.0e19;
        return (long) big; // < Long.MIN_VALUE → -9223372036854775808
    }

    public static int testF2iNaN() {
        float nan = Float.NaN;
        return (int) nan; // 0
    }

    public static int testF2iPosOverflow() {
        float big = 3.0e10f;
        return (int) big; // 2147483647
    }

    public static long testF2lNaN() {
        float nan = Float.NaN;
        return (long) nan; // 0L
    }

    // --- Normal conversions ---

    public static int testD2iNormal() {
        double d = 42.7;
        return (int) d; // 42 (truncate toward zero)
    }

    public static long testD2lNormal() {
        double d = 123456789.9;
        return (long) d; // 123456789
    }

    public static int testF2iNormal() {
        float f = -7.9f;
        return (int) f; // -7 (truncate toward zero)
    }

    public static long testF2lNormal() {
        float f = 100000.5f;
        return (long) f; // 100000
    }

    // --- Math.floor / ceil / rint ---

    public static long testMathFloor() {
        return (long) Math.floor(2.7); // 2
    }

    public static long testMathFloorNeg() {
        return (long) Math.floor(-2.3); // -3
    }

    public static long testMathCeil() {
        return (long) Math.ceil(2.3); // 3
    }

    public static long testMathCeilNeg() {
        return (long) Math.ceil(-2.7); // -2
    }

    public static long testMathRint() {
        return (long) Math.rint(2.5); // 2 (banker's rounding: nearest even)
    }

    public static long testMathRintOdd() {
        return (long) Math.rint(3.5); // 4 (banker's rounding: nearest even)
    }

    // --- Math.abs ---

    public static long testMathAbsDouble() {
        return (long) Math.abs(-42.0); // 42
    }

    public static long testMathAbsDoublePos() {
        return (long) Math.abs(99.0); // 99
    }

    public static int testMathAbsInt() {
        return Math.abs(-123); // 123
    }

    public static int testMathAbsIntPos() {
        return Math.abs(456); // 456
    }

    public static long testMathAbsLong() {
        return Math.abs(-9876543210L); // 9876543210
    }

    public static long testMathAbsLongPos() {
        return Math.abs(1234567890L); // 1234567890
    }

    public static int testMathAbsFloat() {
        float f = Math.abs(-3.14f);
        return (int)(f * 100); // 314
    }

    // --- Combined: N-Body style computation ---

    public static long testNBodyStyle() {
        double dx = 3.0;
        double dy = 4.0;
        double dist = Math.sqrt(dx * dx + dy * dy); // 5.0
        double floored = Math.floor(dist); // 5.0
        return (long) floored; // 5
    }

    public static void main(String[] args) {
        // NaN/overflow conversions
        check("d2i_nan", testD2iNaN(), 0);
        check("d2i_pos_overflow", testD2iPosOverflow(), 2147483647);
        check("d2i_neg_overflow", testD2iNegOverflow(), -2147483648);
        check("d2l_nan", testD2lNaN(), 0L);
        check("d2l_pos_overflow", testD2lPosOverflow(), 9223372036854775807L);
        check("d2l_neg_overflow", testD2lNegOverflow(), -9223372036854775808L);
        check("f2i_nan", testF2iNaN(), 0);
        check("f2i_pos_overflow", testF2iPosOverflow(), 2147483647);
        check("f2l_nan", testF2lNaN(), 0L);

        // Normal conversions
        check("d2i_normal", testD2iNormal(), 42);
        check("d2l_normal", testD2lNormal(), 123456789L);
        check("f2i_normal", testF2iNormal(), -7);
        check("f2l_normal", testF2lNormal(), 100000L);

        // Math.floor/ceil/rint
        check("floor", testMathFloor(), 2L);
        check("floor_neg", testMathFloorNeg(), -3L);
        check("ceil", testMathCeil(), 3L);
        check("ceil_neg", testMathCeilNeg(), -2L);
        check("rint_even", testMathRint(), 2L);
        check("rint_odd", testMathRintOdd(), 4L);

        // Math.abs
        check("abs_double", testMathAbsDouble(), 42L);
        check("abs_double_pos", testMathAbsDoublePos(), 99L);
        check("abs_int", testMathAbsInt(), 123);
        check("abs_int_pos", testMathAbsIntPos(), 456);
        check("abs_long", testMathAbsLong(), 9876543210L);
        check("abs_long_pos", testMathAbsLongPos(), 1234567890L);
        check("abs_float", testMathAbsFloat(), 314);

        // Combined
        check("nbody_style", testNBodyStyle(), 5L);

        System.out.println("ALL FP COMPLETENESS TESTS PASSED");
    }

    static void check(String name, long got, long expected) {
        if (got != expected) {
            System.out.println("FAIL " + name + ": expected " + expected + " got " + got);
            System.exit(1);
        }
    }
}
