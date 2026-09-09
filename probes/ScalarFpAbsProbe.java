// Behavioural check on `Math.abs(float)` / `Math.abs(double)` lowered as an IR
// scalar intrinsic (ANDPS/ANDPD against a sign mask).
//
// The sign-mask form is chosen over the obvious `x < 0 ? -x : x` precisely
// because of the cases below, and each one is a case where the comparison form
// gives a DIFFERENT answer:
//
//   abs(-0.0)  the JLS says +0.0. The comparison form returns -0.0, because
//              -0.0 < 0 is false. Only distinguishable via 1/x, since
//              -0.0 == +0.0 compares true -- hence the divide in the check.
//   abs(NaN)   must be NaN. Every comparison against NaN is false, so the
//              comparison form returns the NaN unchanged, which happens to be
//              right; the mask form is right for a different reason (clearing
//              the sign of a NaN is still a NaN). Checked anyway.
//   abs(-Inf)  must be +Inf.
//   MIN_VALUE  the subnormal, where an exponent-based trick would go wrong.
//
// Run against HotSpot first: it is the oracle, and this file asserts the same
// answers on both.
public class ScalarFpAbsProbe {
    static int fails = 0;

    static void check(String what, boolean ok) {
        if (!ok) { System.out.println("FAIL " + what); fails++; }
    }

    // Kept out of line and called in a loop so the JIT actually compiles them.
    static float af(float x) { return Math.abs(x); }
    static double ad(double x) { return Math.abs(x); }

    // A summing driver, so the values flow through a compiled body rather than
    // being folded at each call site.
    static double drive(double[] xs) {
        double s = 0;
        for (int i = 0; i < xs.length; i++) s += ad(xs[i]);
        return s;
    }

    static float driveF(float[] xs) {
        float s = 0;
        for (int i = 0; i < xs.length; i++) s += af(xs[i]);
        return s;
    }

    public static void main(String[] args) {
        double[] xs = { -1.5, 2.5, -0.25, 7.0, -3.0 };
        float[] fs = { -1.5f, 2.5f, -0.25f, 7.0f, -3.0f };
        double wantD = 14.25, wantF = 14.25f;

        // Warm past the compile thresholds; then every assertion below is
        // about the COMPILED body.
        double acc = 0;
        for (int r = 0; r < 200000; r++) acc += drive(xs);
        check("drive sum", acc == wantD * 200000);

        float accF = 0;
        for (int r = 0; r < 200000; r++) accF += driveF(fs);
        check("driveF sum", accF > 0);
        check("driveF one pass", driveF(fs) == wantF);

        // Negative zero: +0.0 and -0.0 compare EQUAL, so the sign has to be
        // read out through the reciprocal.
        check("abs(-0.0) is +0.0", 1.0 / ad(-0.0) == Double.POSITIVE_INFINITY);
        check("abs(+0.0) is +0.0", 1.0 / ad(0.0) == Double.POSITIVE_INFINITY);
        check("absF(-0.0f) is +0.0f", 1.0f / af(-0.0f) == Float.POSITIVE_INFINITY);

        check("abs(NaN) is NaN", Double.isNaN(ad(Double.NaN)));
        check("absF(NaN) is NaN", Float.isNaN(af(Float.NaN)));

        check("abs(-Inf)", ad(Double.NEGATIVE_INFINITY) == Double.POSITIVE_INFINITY);
        check("abs(+Inf)", ad(Double.POSITIVE_INFINITY) == Double.POSITIVE_INFINITY);
        check("absF(-Inf)", af(Float.NEGATIVE_INFINITY) == Float.POSITIVE_INFINITY);

        check("abs(-MIN_VALUE)", ad(-Double.MIN_VALUE) == Double.MIN_VALUE);
        check("absF(-MIN_VALUE)", af(-Float.MIN_VALUE) == Float.MIN_VALUE);
        check("abs(-MAX_VALUE)", ad(-Double.MAX_VALUE) == Double.MAX_VALUE);

        // The sign mask must not disturb the mantissa of an ordinary value.
        check("abs(-1.0000000000000002)",
              ad(-1.0000000000000002) == 1.0000000000000002);

        System.out.println(fails == 0 ? "FP ABS PROBE OK" : "FP ABS PROBE FAILED " + fails);
    }
}
