package cratonvm;

/**
 * Differential fixture for {@code Double.toString} / {@code Float.toString}
 * layout: the {@code 10^-3 .. 10^7} plain-decimal window, the scientific form
 * outside it, subnormals, and the specials — exercised through every path that
 * turns a floating-point value into text (println, String.valueOf, the boxed
 * toString, StringBuilder.append and string concatenation).
 */
public class DiffFloatFormat {

    private static final double[] DOUBLES = {
        3.14, 0.1 + 0.2, 1e7, 9999999.0, 1e-4, 0.001, 0.0, -0.0,
        1.0 / 0.0, -1.0 / 0.0, 0.0 / 0.0, 1234567.0, 12345678.0,
        1e300, 1e-300, 4.9e-324, 100.0, 1.0 / 3.0, 2.5e-5,
        Double.MAX_VALUE, Double.MIN_VALUE, Double.MIN_NORMAL,
    };

    private static final float[] FLOATS = {
        3.14f, 1e8f, 9999999.0f, 1e-4f, 0.1f, 1.0f / 3.0f, 1.4e-45f,
        100.0f, -0.0f, Float.MAX_VALUE, Float.MIN_VALUE,
        Float.POSITIVE_INFINITY, Float.NaN,
    };

    public static void main(String[] args) {
        for (double d : DOUBLES) {
            System.out.println(d);
            System.out.println("valueOf " + String.valueOf(d));
            System.out.println("toString " + Double.toString(d));
            System.out.println("boxed " + Double.valueOf(d).toString());
            System.out.println("concat " + d + "|");
            System.out.println("sb " + new StringBuilder().append(d).toString());
        }
        for (float f : FLOATS) {
            System.out.println(f);
            System.out.println("fvalueOf " + String.valueOf(f));
            System.out.println("ftoString " + Float.toString(f));
            System.out.println("fboxed " + Float.valueOf(f).toString());
            System.out.println("fconcat " + f + "|");
            System.out.println("fsb " + new StringBuilder().append(f).toString());
        }
    }
}
