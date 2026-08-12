/**
 * W7-44: isolate the `Random.nextGaussian` divergence to the `log` call.
 *
 * `java.util.Random.nextGaussian()` on JDK 25 is still the classic polar
 * method and reads:
 *
 *     multiplier = StrictMath.sqrt(-2 * StrictMath.log(s) / s);
 *
 * Every operation in that line is exactly-rounded IEEE arithmetic EXCEPT
 * `StrictMath.log`, which is contractually fdlibm. CratonVM's polar method uses
 * Rust's `f64::ln` (platform libm). This probe recomputes the first variate of
 * `new Random(42)` twice — once through `StrictMath.log`, once through
 * `Math.log` (the HotSpot intrinsic, i.e. a NON-fdlibm log) — and prints the raw
 * bits of each. If the two differ, and the `Math.log` answer equals what
 * CratonVM prints, the divergence is a `log` last-ULP difference and neither
 * `nextGaussian`'s structure nor `Double.toString` is implicated.
 *
 * Run:  java probes/GaussianLogUlpProbe.java
 */
public final class GaussianLogUlpProbe {

    public static void main(String[] args) {
        // `new Random(42)`: initialScramble + next(bits), verbatim.
        long seed = (42L ^ 0x5DEECE66DL) & ((1L << 48) - 1);
        long[] state = { seed };

        double v1;
        double v2;
        double s;
        do {
            v1 = 2 * nextDouble(state) - 1;
            v2 = 2 * nextDouble(state) - 1;
            s = v1 * v1 + v2 * v2;
        } while (s >= 1 || s == 0);

        System.out.println("s.bits=" + Long.toHexString(Double.doubleToRawLongBits(s)) + " s=" + s);
        double strictLog = StrictMath.log(s);
        double mathLog = Math.log(s);
        System.out.println("StrictMath.log(s).bits=" + Long.toHexString(Double.doubleToRawLongBits(strictLog)));
        System.out.println("Math.log(s).bits=" + Long.toHexString(Double.doubleToRawLongBits(mathLog)));
        System.out.println("log.differs=" + (Double.doubleToRawLongBits(strictLog)
                != Double.doubleToRawLongBits(mathLog)));

        double mStrict = StrictMath.sqrt(-2 * strictLog / s);
        double mMath = Math.sqrt(-2 * mathLog / s);
        double g1Strict = v1 * mStrict;
        double g1Math = v1 * mMath;
        System.out.println("g1.viaStrictLog.bits=" + Long.toHexString(Double.doubleToRawLongBits(g1Strict))
                + " str=" + g1Strict);
        System.out.println("g1.viaMathLog.bits=" + Long.toHexString(Double.doubleToRawLongBits(g1Math))
                + " str=" + g1Math);
        System.out.println("g2.viaStrictLog=" + (v2 * mStrict));
        System.out.println("g2.viaMathLog=" + (v2 * mMath));

        // How often do the two logs disagree at all, over the domain the polar
        // method actually samples ((0,1) — where `log` is negative)?
        java.util.Random r = new java.util.Random(7);
        int disagree = 0;
        int n = 1000000;
        for (int i = 0; i < n; i++) {
            double x = r.nextDouble();
            if (x == 0) {
                continue;
            }
            if (Double.doubleToRawLongBits(StrictMath.log(x))
                    != Double.doubleToRawLongBits(Math.log(x))) {
                disagree++;
            }
        }
        System.out.println("log.disagreeRate=" + disagree + "/" + n);
    }

    private static int next(long[] state, int bits) {
        state[0] = (state[0] * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
        return (int) (state[0] >>> (48 - bits));
    }

    private static double nextDouble(long[] state) {
        return (((long) next(state, 26) << 27) + next(state, 27)) * 0x1.0p-53;
    }
}
