import java.math.BigDecimal;
import java.text.ChoiceFormat;
import java.text.MessageFormat;
import java.util.Arrays;
import java.util.Locale;

/**
 * Tomcat's org.apache.el.util.TestMessageFactory#testFormatChoice, reduced.
 *
 * The bundle entry is
 *   {0,choice,0#{0,number} is too few|99#{0,number} is enough|100<{0,number} is too many}
 * and the argument is new BigDecimal("1E+2"), i.e. 100 -> "100 is enough".
 *
 * ChoiceFormat keeps its thresholds in a double[] `choiceLimits`. If that array
 * reads back all zeroes, every limit compares <= 100 and the LAST branch wins,
 * which is the "100 is too many" the Tomcat suite recorded under -XX:+UseZGC.
 */
public class ChoiceFormatProbe {

	static int failures = 0;

	static void check(String what, Object got, Object want) {
		boolean ok = String.valueOf(got).equals(String.valueOf(want));
		System.out.println((ok ? "  ok   " : "  FAIL ") + what + " = " + got + (ok ? "" : "   (want " + want + ")"));
		if (!ok) {
			failures++;
		}
	}

	public static void main(String[] args) {
		String pattern = "{0,choice,0#{0,number} is too few|99#{0,number} is enough|100<{0,number} is too many}";
		MessageFormat mf = new MessageFormat(pattern, Locale.ENGLISH);
		check("MessageFormat choice, BigDecimal 1E+2",
				mf.format(new Object[] { new BigDecimal("1E+2") }), "100 is enough");
		check("MessageFormat choice, int 100", mf.format(new Object[] { Integer.valueOf(100) }), "100 is enough");
		check("MessageFormat choice, int 5", mf.format(new Object[] { Integer.valueOf(5) }), "5 is too few");
		check("MessageFormat choice, int 500", mf.format(new Object[] { Integer.valueOf(500) }), "500 is too many");

		// The array underneath, read directly: this is what a zeroed double[]
		// would expose, and it is the difference between the two readings.
		ChoiceFormat cf = new ChoiceFormat("0#too few|99#enough|100<too many");
		check("ChoiceFormat.getLimits()", Arrays.toString(cf.getLimits()),
				Arrays.toString(new double[] { 0.0, 99.0, ChoiceFormat.nextDouble(100.0) }));
		check("ChoiceFormat.getFormats()", Arrays.toString((Object[]) cf.getFormats()),
				"[too few, enough, too many]");
		check("ChoiceFormat.format(100)", cf.format(100.0), "enough");

		// setChoices round-trip: the same double[] handed in and read back.
		ChoiceFormat set = new ChoiceFormat(new double[] { 0.0, 99.0, 100.0 },
				new String[] { "too few", "enough", "too many" });
		check("setChoices limits round-trip", Arrays.toString(set.getLimits()), "[0.0, 99.0, 100.0]");
		check("setChoices format(99.5)", set.format(99.5), "enough");

		System.out.println(failures == 0 ? "PROBE-OK" : "PROBE-FAILURES=" + failures);
		System.exit(failures == 0 ? 0 : 1);
	}

}
