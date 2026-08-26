import java.lang.reflect.*;

/** StrictMath, bit-for-bit, against whatever VM runs it.
 *
 *  StrictMath is the one class whose SPEC requires a specific algorithm: its
 *  results must match the fdlibm-derived reference exactly, not to within an
 *  ulp. (java.lang.Math is the one allowed 1-ulp slack.) So any difference at
 *  all between two VMs is a conformance defect, and printing raw bits is the
 *  only way to see it -- toString() rounds the evidence away.
 *
 *  Output is one line per (method, input) so two runs can be diffed on stdout. */
public class StrictMathBits {
    static final double[] XS = {
        0.0, -0.0, 1.0, -1.0, 0.5, -0.5, 2.0, 3.0, 10.0, 100.0,
        0.1, 0.2, 0.3, 1e-8, 1e-300, 1e300, 1234.5678, -1234.5678,
        Math.PI, Math.PI/2, Math.PI/4, Math.E, 0.7853981633974483,
        1e-16, 123456789.123456789, 2.2250738585072014E-308,
        Double.MIN_VALUE, Double.MAX_VALUE,
    };
    static void p(String n, double x, double r) {
        System.out.println(n + " " + Double.doubleToRawLongBits(x) + " -> "
                           + Double.doubleToRawLongBits(r));
    }
    public static void main(String[] a) {
        for (double x : XS) {
            p("sin", x, StrictMath.sin(x));
            p("cos", x, StrictMath.cos(x));
            p("tan", x, StrictMath.tan(x));
            p("asin", x, StrictMath.asin(x));
            p("acos", x, StrictMath.acos(x));
            p("atan", x, StrictMath.atan(x));
            p("exp", x, StrictMath.exp(x));
            p("log", x, StrictMath.log(x));
            p("log10", x, StrictMath.log10(x));
            p("log1p", x, StrictMath.log1p(x));
            p("expm1", x, StrictMath.expm1(x));
            p("sqrt", x, StrictMath.sqrt(x));
            p("cbrt", x, StrictMath.cbrt(x));
            p("sinh", x, StrictMath.sinh(x));
            p("cosh", x, StrictMath.cosh(x));
            p("tanh", x, StrictMath.tanh(x));
            p("ceil", x, StrictMath.ceil(x));
            p("floor", x, StrictMath.floor(x));
            p("rint", x, StrictMath.rint(x));
            p("ulp", x, StrictMath.ulp(x));
            p("signum", x, StrictMath.signum(x));
            p("toDegrees", x, StrictMath.toDegrees(x));
            p("toRadians", x, StrictMath.toRadians(x));
            p("nextUp", x, StrictMath.nextUp(x));
            p("nextDown", x, StrictMath.nextDown(x));
            for (double y : new double[]{1.0, 2.0, 0.5, -3.0, 1e10, 0.1}) {
                System.out.println("pow " + Double.doubleToRawLongBits(x) + ","
                    + Double.doubleToRawLongBits(y) + " -> "
                    + Double.doubleToRawLongBits(StrictMath.pow(x, y)));
                System.out.println("atan2 " + Double.doubleToRawLongBits(x) + ","
                    + Double.doubleToRawLongBits(y) + " -> "
                    + Double.doubleToRawLongBits(StrictMath.atan2(x, y)));
                System.out.println("hypot " + Double.doubleToRawLongBits(x) + ","
                    + Double.doubleToRawLongBits(y) + " -> "
                    + Double.doubleToRawLongBits(StrictMath.hypot(x, y)));
                System.out.println("IEEErem " + Double.doubleToRawLongBits(x) + ","
                    + Double.doubleToRawLongBits(y) + " -> "
                    + Double.doubleToRawLongBits(StrictMath.IEEEremainder(x, y)));
                System.out.println("copySign " + Double.doubleToRawLongBits(x) + ","
                    + Double.doubleToRawLongBits(y) + " -> "
                    + Double.doubleToRawLongBits(StrictMath.copySign(x, y)));
                System.out.println("nextAfter " + Double.doubleToRawLongBits(x) + ","
                    + Double.doubleToRawLongBits(y) + " -> "
                    + Double.doubleToRawLongBits(StrictMath.nextAfter(x, y)));
            }
        }
        System.out.println("DONE StrictMathBits");
    }
}
