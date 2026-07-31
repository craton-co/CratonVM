import java.text.DecimalFormat;
import java.text.NumberFormat;
import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;
import java.util.TimeZone;

/**
 * Localises known-issue 30.A's 225 us `SimpleDateFormat.format` to individual
 * pattern letters, and pins the `DecimalFormat.format(int)` outlier that the
 * chain decomposition turned up (39 us against HotSpot's 109 ns).
 *
 * `SimpleDateFormat.subFormat` dispatches per pattern letter: numeric fields
 * (`d`, `y`, `H`, `m`, `s`) go through `zeroPaddingNumber`, which has a fast
 * path for 1-2 digit values and otherwise falls back to `NumberFormat.format`;
 * text fields (`MMM`) index `DateFormatSymbols`. Timing one letter at a time
 * says which dispatch arm is expensive without guessing.
 *
 * Every stage is a static method with the loop inline — no lambdas.
 */
public final class DateFormatPatternProbe {

    private static final Locale US = Locale.US;
    private static final Date DATE = new Date(1_700_000_000_000L);

    private static SimpleDateFormat[] FMT;
    private static String[] PATTERNS = {
        "d",        // 1-digit numeric, fast path
        "dd",       // 2-digit numeric, fast path
        "yyyy",     // 4-digit numeric -> NumberFormat fallback?
        "MMM",      // text, DateFormatSymbols lookup
        "MM",       // 2-digit numeric month
        "HH",
        "mm",
        "ss",
        "HH:mm:ss",
        "dd-MMM-yyyy",
        "dd-MMM-yyyy HH:mm:ss",   // the real one
    };

    private static DecimalFormat DEC2;
    private static NumberFormat NF;
    private static long sink;

    private static void formatWith(int idx, int n) {
        SimpleDateFormat f = FMT[idx];
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += f.format(DATE).length();
        }
        sink += s;
    }

    private static void decFormatInt(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += DEC2.format(i % 60).length();
        }
        sink += s;
    }

    private static void decFormatLong(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += DEC2.format((long) (i % 60)).length();
        }
        sink += s;
    }

    private static void nfGetIntegerInstance(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += NumberFormat.getIntegerInstance(US).hashCode() & 1;
        }
        sink += s;
    }

    private static void integerToString(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += Integer.toString(i % 60).length();
        }
        sink += s;
    }

    private static void sbAppendInt(int n) {
        StringBuilder sb = new StringBuilder(8);
        long s = 0;
        for (int i = 0; i < n; i++) {
            sb.setLength(0);
            sb.append(i % 60);
            s += sb.length();
        }
        sink += s;
    }

    public static void main(String[] args) {
        int blocks = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        int bs = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;

        FMT = new SimpleDateFormat[PATTERNS.length];
        for (int i = 0; i < PATTERNS.length; i++) {
            FMT[i] = new SimpleDateFormat(PATTERNS[i], US);
            FMT[i].setTimeZone(TimeZone.getDefault());
            FMT[i].format(DATE); // warm / force clinit
        }
        DEC2 = (DecimalFormat) NumberFormat.getIntegerInstance(US);
        DEC2.setMinimumIntegerDigits(2);
        DEC2.setGroupingUsed(false);
        NF = NumberFormat.getIntegerInstance(US);

        System.out.printf("%-26s", "pattern (ns/op by block)");
        for (int b = 0; b < blocks; b++) {
            System.out.printf("%10d", b);
        }
        System.out.println();

        for (int i = 0; i < PATTERNS.length; i++) {
            StringBuilder out = new StringBuilder(String.format("  \"%-22s", PATTERNS[i] + "\""));
            for (int b = 0; b < blocks; b++) {
                long t0 = System.nanoTime();
                formatWith(i, bs);
                out.append(String.format("%10d", (System.nanoTime() - t0) / bs));
            }
            System.out.println(out);
        }

        System.out.println();
        String[] others = {"decFormatInt", "decFormatLong", "integerToString",
                "sbAppendInt", "nfGetIntegerInstance"};
        for (int k = 0; k < others.length; k++) {
            StringBuilder out = new StringBuilder(String.format("%-26s", others[k]));
            for (int b = 0; b < blocks; b++) {
                long t0 = System.nanoTime();
                switch (k) {
                    case 0: decFormatInt(bs); break;
                    case 1: decFormatLong(bs); break;
                    case 2: integerToString(bs); break;
                    case 3: sbAppendInt(bs); break;
                    default: nfGetIntegerInstance(bs / 10); break;
                }
                long per = (System.nanoTime() - t0) / (k == 4 ? bs / 10 : bs);
                out.append(String.format("%10d", per));
            }
            System.out.println(out);
        }
        System.out.println("sink=" + sink);
    }
}
