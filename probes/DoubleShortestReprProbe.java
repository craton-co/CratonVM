import java.util.Random;

/**
 * W7-44 measurement probe for the `Random.nextGaussian` divergence
 * (HotSpot `1.1419053154730547` vs CratonVM `1.141905315473055`).
 *
 * The question this probe answers is NOT "what does nextGaussian print" but
 * "are the two sides the same double". It prints `doubleToRawLongBits` beside
 * `Double.toString` for the probe's own value and for a spread of doubles whose
 * shortest-round-trip decimal is known to be where Java's rule and Rust's
 * `{}` / ryu rules can disagree.
 *
 * Read the output like this:
 *   - `gaussian.bits` identical on both sides + `gaussian.str` different
 *       => `nextGaussian` is innocent; the defect is in `Double.toString`.
 *   - `gaussian.bits` different
 *       => `nextGaussian` computes a different value.
 *
 * Run:  java probes/DoubleShortestReprProbe.java
 */
public final class DoubleShortestReprProbe {

    public static void main(String[] args) {
        double g = new Random(42).nextGaussian();
        System.out.println("gaussian.bits=" + Long.toHexString(Double.doubleToRawLongBits(g)));
        System.out.println("gaussian.str=" + g);
        System.out.println("gaussian.roundTripsFromStr="
                + (Double.doubleToRawLongBits(Double.parseDouble(Double.toString(g)))
                        == Double.doubleToRawLongBits(g)));

        // The first eight draws of the same stream: if the algorithm diverges
        // at all, it will not diverge only on draw #1.
        Random r = new Random(42);
        for (int i = 0; i < 8; i++) {
            double d = r.nextGaussian();
            System.out.println("gaussian[" + i + "].bits="
                    + Long.toHexString(Double.doubleToRawLongBits(d))
                    + " str=" + d);
        }

        // The underlying long stream, so a divergence can be localised to the
        // bit source rather than the gaussian transform.
        Random r2 = new Random(42);
        StringBuilder longs = new StringBuilder();
        for (int i = 0; i < 4; i++) {
            longs.append(Long.toHexString(r2.nextLong())).append(' ');
        }
        System.out.println("nextLong.seed42=" + longs.toString().trim());

        // A census of doubles whose shortest representation is a classic
        // Java-vs-others disagreement point.
        double[] xs = {
            1.1419053154730547,
            1.141905315473055,
            0.1, 0.2, 0.3, 0.1 + 0.2,
            1.0 / 3.0, 2.0 / 3.0,
            1e23, 9.999999999999999e22,
            1.0e-323, Double.MIN_VALUE, Double.MAX_VALUE,
            1e7, 1e-3, 1.0e21, 1.0e-7,
            123456789.0, 1234567890123456789.0,
            2.2250738585072014e-308,
            5e-324, 4.9e-324,
            0.0, -0.0, 1.0, -1.0, 100.0,
            3.141592653589793, 2.718281828459045,
            1.7976931348623157e308,
            0.001, 0.0001,
            8.98846567431158e307,
            1.0e16, 1.0e17, 9007199254740992.0,
        };
        for (double x : xs) {
            System.out.println("d.bits=" + Long.toHexString(Double.doubleToRawLongBits(x))
                    + " str=" + Double.toString(x)
                    + " float=" + Float.toString((float) x));
        }

        // Randomised round-trip census: every double must print in a form that
        // parses back to the same bits, and the shortest such form is what Java
        // specifies. A mismatch count > 0 on either side is a Double.toString bug.
        Random rr = new Random(12345);
        int roundTripFailures = 0;
        int shorterExists = 0;
        for (int i = 0; i < 200000; i++) {
            double d = Double.longBitsToDouble(rr.nextLong());
            if (Double.isNaN(d) || Double.isInfinite(d)) {
                continue;
            }
            String s = Double.toString(d);
            if (Double.doubleToRawLongBits(Double.parseDouble(s)) != Double.doubleToRawLongBits(d)) {
                roundTripFailures++;
            }
            if (hasShorterRoundTrip(d, s)) {
                shorterExists++;
            }
        }
        System.out.println("census.roundTripFailures=" + roundTripFailures);
        System.out.println("census.nonShortest=" + shorterExists);
    }

    /**
     * True when some decimal with FEWER significant digits than {@code s} also
     * round-trips to {@code d} — i.e. {@code s} is not the shortest form.
     */
    private static boolean hasShorterRoundTrip(double d, String s) {
        int digits = significantDigits(s);
        for (int n = 1; n < digits; n++) {
            String cand = new java.math.BigDecimal(d).round(
                    new java.math.MathContext(n)).toString();
            if (Double.doubleToRawLongBits(Double.parseDouble(cand))
                    == Double.doubleToRawLongBits(d)) {
                return true;
            }
        }
        return false;
    }

    private static int significantDigits(String s) {
        int e = s.indexOf('E');
        String mant = e < 0 ? s : s.substring(0, e);
        int n = 0;
        boolean seenNonZero = false;
        for (int i = 0; i < mant.length(); i++) {
            char c = mant.charAt(i);
            if (c >= '1' && c <= '9') {
                seenNonZero = true;
                n++;
            } else if (c == '0' && seenNonZero) {
                n++;
            }
        }
        // trailing ".0" that Java always emits is not significant
        if (mant.endsWith(".0") && n > 1) {
            n--;
        }
        return Math.max(n, 1);
    }
}
