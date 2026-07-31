import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;
import java.util.TimeZone;

/**
 * `SimpleDateFormat.format` and NOTHING else, so `CRATONVM_DBG=jit-method-stats`
 * attributes cleanly.
 *
 * DateFormatChainProbe/DateFormatPatternProbe also drive `DecimalFormat.format`
 * and `NumberFormat.getIntegerInstance` directly, so their invocation counts
 * cannot distinguish "SimpleDateFormat calls this" from "the probe calls this".
 * This one only ever calls `format`.
 *
 * Pattern is chosen by -Dsdf.pattern (default "ss" — a single 2-digit numeric
 * field, the cheapest thing SimpleDateFormat can do).
 */
public final class SdfOnlyProbe {
    private static long sink;

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 50_000;
        String pattern = System.getProperty("sdf.pattern", "ss");

        SimpleDateFormat f = new SimpleDateFormat(pattern, Locale.US);
        f.setTimeZone(TimeZone.getDefault());
        Date d = new Date(1_700_000_000_000L);
        f.format(d);

        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += f.format(d).length();
        }
        long dt = System.nanoTime() - t0;
        System.out.println("pattern=\"" + pattern + "\" n=" + n
                + " total=" + dt + " ns  per-op=" + (dt / n) + " ns  sink=" + sink);
    }
}
