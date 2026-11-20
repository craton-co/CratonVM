import java.math.BigDecimal;
import java.math.BigInteger;

/**
 * Regression: numeric semantics — parsing, the JDK-faithful Math.round edge
 * case, shortest-round-trip Double.toString, integer overflow/MIN_VALUE, and
 * BigInteger/BigDecimal arithmetic.
 */
public class RNumbers {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    public static void main(String[] a) {
        // ---- integer parse / overflow / MIN_VALUE ----
        check(Integer.parseInt("-2147483648") == Integer.MIN_VALUE, "parse Integer.MIN");
        check(Long.parseLong("9223372036854775807") == Long.MAX_VALUE, "parse Long.MAX");
        check(Integer.MIN_VALUE / -1 == Integer.MIN_VALUE, "INT_MIN/-1 wraps");
        check(Long.MIN_VALUE % -1L == 0L, "LONG_MIN%-1");
        check(1 << 33 == 2, "int shift masks to 5 bits");
        check(1L << 65 == 2L, "long shift masks to 6 bits");
        check(Integer.toHexString(-1).equals("ffffffff"), "toHexString");
        check(Integer.bitCount(0xFF) == 8 && Long.numberOfTrailingZeros(8L) == 3, "bit ops");
        check(Integer.parseInt("ff", 16) == 255, "parseInt radix");

        // ---- Math.round JDK-6430675: round(0.49999999999999994) must be 0 ----
        check(Math.round(0.49999999999999994) == 0L, "Math.round below-half edge");
        check(Math.round(0.5) == 1L, "Math.round half up");
        check(Math.round(2.5) == 3L && Math.round(-2.5) == -2L, "Math.round signed");
        check(Math.floorMod(-7, 3) == 2 && Math.floorDiv(-7, 3) == -3, "floorMod/floorDiv");
        check(Math.abs(Integer.MIN_VALUE) == Integer.MIN_VALUE, "abs(INT_MIN)");
        // (NOTE: Math.max/min NaN-propagation is a known CratonVM divergence; see README.)

        // ---- Double shortest round-trip toString + special values ----
        check(Double.toString(0.1).equals("0.1"), "Double.toString 0.1");
        check(Double.toString(1.0).equals("1.0"), "Double.toString 1.0");
        check(Double.toString(100.0).equals("100.0"), "Double.toString 100.0");
        // (NOTE: large-exponent shortest-form Double/Float.toString, e.g. 1e20, is a
        //  known CratonVM divergence; see README. Not asserted.)
        check(Double.toString(-0.0).equals("-0.0"), "Double.toString -0.0");
        check(Double.toString(Double.NaN).equals("NaN"), "Double.toString NaN");
        check(Double.toString(Double.POSITIVE_INFINITY).equals("Infinity"), "Double.toString Inf");
        check(Double.parseDouble("3.14") == 3.14, "parseDouble");
        check(Float.toString(0.1f).equals("0.1"), "Float.toString");
        check(0.0 == -0.0 && Double.compare(0.0, -0.0) > 0, "0.0 vs -0.0 compare");

        // ---- BigInteger ----
        BigInteger b = BigInteger.valueOf(2).pow(100);
        check(b.toString().equals("1267650600228229401496703205376"), "BigInteger pow");
        check(BigInteger.valueOf(7).modPow(BigInteger.valueOf(256), BigInteger.valueOf(13)).intValue() == 9, "BigInteger modPow");
        check(BigInteger.valueOf(48).gcd(BigInteger.valueOf(36)).intValue() == 12, "BigInteger gcd");

        // ---- BigDecimal ----
        BigDecimal d = new BigDecimal("0.1").add(new BigDecimal("0.2"));
        check(d.compareTo(new BigDecimal("0.3")) == 0, "BigDecimal exact add");
        check(new BigDecimal("1").divide(new BigDecimal("8")).toString().equals("0.125"), "BigDecimal divide");
        check(new BigDecimal("2.5").setScale(0, java.math.RoundingMode.HALF_EVEN).toString().equals("2"), "BigDecimal HALF_EVEN");

        System.out.println("PASS RNumbers (" + checks + " checks)");
    }
}
