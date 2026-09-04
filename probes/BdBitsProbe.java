import java.math.BigDecimal;
import java.math.BigInteger;

/**
 * G10-1's single divergence, split into its two possible causes.
 *
 * dv.9007199254740993.1 renders as ...992E14 on HotSpot and ...993E14 here.
 * The probe prints the value through TWO independent routes: the raw IEEE-754
 * bits, and Double.toString. If the bits agree, doubleValue() is correct and
 * the defect is in the RENDERING, which is a much broader surface.
 */
public class BdBitsProbe {
    static void show(String label, double d) {
        System.out.println(label + ".toString = " + d);
        System.out.println(label + ".rawBits  = " + Long.toHexString(Double.doubleToRawLongBits(d)));
    }

    public static void main(String[] a) {
        BigDecimal bd = new BigDecimal(BigInteger.valueOf(9007199254740993L), 1);
        System.out.println("bd.unscaled = " + bd.unscaledValue());
        System.out.println("bd.scale    = " + bd.scale());
        System.out.println("bd.toString = " + bd);
        show("bd.doubleValue", bd.doubleValue());

        // The same double reached without BigDecimal at all: if this row also
        // renders ...993 then the parser is fine and Double.toString is not.
        show("parseDouble", Double.parseDouble("900719925474099.3"));
        show("literal", 900719925474099.3d);

        // A control the record already measured: scale 0 must be ...992E15.
        show("bd0.doubleValue",
                new BigDecimal(BigInteger.valueOf(9007199254740993L), 0).doubleValue());

        // Round-trip: does our own toString parse back to the same bits?
        double v = bd.doubleValue();
        double rt = Double.parseDouble(Double.toString(v));
        System.out.println("roundTripsToSameBits = "
                + (Double.doubleToRawLongBits(v) == Double.doubleToRawLongBits(rt)));
        System.out.println("RESULT done");
    }
}
