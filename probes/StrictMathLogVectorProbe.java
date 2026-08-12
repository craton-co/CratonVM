import java.util.ArrayList;
import java.util.List;
import java.util.Random;

/**
 * Emits the golden `(input bits, StrictMath.log(input) bits)` table used by
 * `types/src/fdlibm.rs`. Run on a reference HotSpot and paste the stdout into
 * that module's `VECTORS` constant — the table's value is that it is the
 * reference implementation's output, so it must never be edited by hand.
 *
 *   java probes/StrictMathLogVectorProbe.java > vectors.txt
 *
 * stderr carries a count of how many of the emitted inputs `Math.log` (the
 * HotSpot intrinsic, i.e. a non-fdlibm log) disagrees with — a nonzero count is
 * the reason the port exists. See W7-44-numberformat-enum-and-double-tostring.md.
 */
public final class StrictMathLogVectorProbe {

    public static void main(String[] args) {
        List<Double> xs = new ArrayList<>();
        double[] fixed = {
            // The `s` that `new Random(42)`'s first accepted polar pair produces.
            0.3414242762953298,
            1.0, 2.0, 0.5, Math.E, 10.0, 0.1, 1e-300, 1e300,
            Double.MIN_VALUE, Double.MIN_NORMAL, Double.MAX_VALUE,
            // Both sides of 1.0, where fdlibm takes its |f| < 2^-20 branch.
            0.9999999999999999, 1.0000000000000002, 1.9999999999999998,
            // The argument-reduction boundary.
            1.4142135623730951, 0.7071067811865476,
            3.0, 100.0, 1e-10, 1e10,
            0.0, -0.0, -1.0,
            Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY,
            5e-324, 2.2250738585072011e-308,
        };
        for (double d : fixed) {
            xs.add(d);
        }
        Random r = new Random(20260812L);
        for (int i = 0; i < 40; i++) {
            xs.add(r.nextDouble());
        }
        for (int i = 0; i < 20; i++) {
            xs.add(Double.longBitsToDouble(r.nextLong() & 0x7fefffffffffffffL));
        }

        int mismatch = 0;
        for (double x : xs) {
            long xb = Double.doubleToRawLongBits(x);
            long yb = Double.doubleToRawLongBits(StrictMath.log(x));
            System.out.printf("        (0x%016X, 0x%016X),%n", xb, yb);
            if (Double.doubleToRawLongBits(Math.log(x)) != yb) {
                mismatch++;
            }
        }
        System.err.println("vectors=" + xs.size() + " differFromMathLog=" + mismatch);
    }
}
