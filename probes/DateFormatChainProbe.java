import java.text.DecimalFormat;
import java.text.DateFormatSymbols;
import java.text.FieldPosition;
import java.text.NumberFormat;
import java.text.SimpleDateFormat;
import java.util.Calendar;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.Locale;
import java.util.TimeZone;

/**
 * Decomposes `SimpleDateFormat.format(Date)` — the miss path of Tomcat's
 * `DateFormatCache`, and the whole of known-issue 30.A.
 *
 * 30.A measures `SimpleDateFormat.format` at 250-300 us against HotSpot's
 * 2.3-2.5 us, with BOTH hot methods compiling. A 100x gap on compiled code is
 * not general slowness — this VM runs arithmetic at parity and allocation at
 * 20-65x — so something specific dominates. This finds which.
 *
 * Every stage is a plain static method with the loop INLINE. Driving stages
 * through a functional interface measures ~3.4 us per call on CratonVM and
 * buries everything it is meant to compare.
 *
 * Compare COLUMNS (HotSpot vs CratonVM) per row, and the deltas BETWEEN rows
 * within one column. Rows are ordered inner-to-outer so a jump names its own
 * frame.
 */
public final class DateFormatChainProbe {

    private static final String PATTERN = "dd-MMM-yyyy HH:mm:ss";
    private static final Locale US = Locale.US;

    private static SimpleDateFormat SDF;
    private static Calendar CAL;
    private static DateFormatSymbols SYMS;
    private static DecimalFormat DEC;
    private static final Date DATE = new Date();
    private static final StringBuffer SB = new StringBuffer(64);

    private static long sink;
    private static Object refSink;

    // ---- stages, inner to outer -------------------------------------------

    private static void dateSetTime(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            DATE.setTime(1_700_000_000_000L + i);
            s += DATE.getTime();
        }
        sink += s;
    }

    /** `Calendar.setTime` forces computeFields — the classic heavy step. */
    private static void calendarSetTimeAndGet(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CAL.setTime(DATE);
            s += CAL.get(Calendar.HOUR_OF_DAY) + CAL.get(Calendar.MINUTE)
                    + CAL.get(Calendar.SECOND) + CAL.get(Calendar.YEAR);
        }
        sink += s;
    }

    /** Just the field reads, no setTime — isolates computeFields from get(). */
    private static void calendarGetOnly(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += CAL.get(Calendar.HOUR_OF_DAY);
        }
        sink += s;
    }

    /** `NumberFormat.format(int)` — SimpleDateFormat's zero-padding path. */
    private static void decimalFormatInt(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += DEC.format(i % 60).length();
        }
        sink += s;
    }

    /** StringBuffer is synchronized — every append takes a monitor. */
    private static void stringBufferAppend(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            SB.setLength(0);
            SB.append('x').append("abc").append(i % 60);
            s += SB.length();
        }
        sink += s;
    }

    private static void symbolsShortMonths(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += SYMS.getShortMonths().length;
        }
        sink += s;
    }

    private static void timeZoneGetDefault(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += TimeZone.getDefault().getRawOffset();
        }
        sink += s;
    }

    /** The whole thing — what 30.A actually measures. */
    private static void sdfFormat(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            DATE.setTime(1_700_000_000_000L + i);
            s += SDF.format(DATE).length();
        }
        sink += s;
    }

    /** Same, but reusing a StringBuffer — skips the result String copy. */
    private static void sdfFormatIntoBuffer(int n) {
        FieldPosition fp = new FieldPosition(0);
        long s = 0;
        for (int i = 0; i < n; i++) {
            DATE.setTime(1_700_000_000_000L + i);
            SB.setLength(0);
            SDF.format(DATE, SB, fp);
            s += SB.length();
        }
        sink += s;
    }

    /** The fast side of 30.A's assertion — a Rust intrinsic on CratonVM. */
    private static void stringFormat(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += String.format("%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS",
                    Long.valueOf(1_700_000_000_000L + i)).length();
        }
        sink += s;
    }

    private static void row(String name, int blocks, int bs, int which) {
        StringBuilder out = new StringBuilder(String.format("%-26s", name));
        for (int b = 0; b < blocks; b++) {
            long t0 = System.nanoTime();
            switch (which) {
                case 0: dateSetTime(bs); break;
                case 1: calendarGetOnly(bs); break;
                case 2: calendarSetTimeAndGet(bs); break;
                case 3: decimalFormatInt(bs); break;
                case 4: stringBufferAppend(bs); break;
                case 5: symbolsShortMonths(bs); break;
                case 6: timeZoneGetDefault(bs); break;
                case 7: sdfFormatIntoBuffer(bs); break;
                case 8: sdfFormat(bs); break;
                default: stringFormat(bs); break;
            }
            out.append(String.format("%10d", (System.nanoTime() - t0) / bs));
        }
        System.out.println(out);
    }

    public static void main(String[] args) {
        int blocks = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int bs = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;

        long t0 = System.nanoTime();
        SDF = new SimpleDateFormat(PATTERN, US);
        SDF.setTimeZone(TimeZone.getDefault());
        System.out.println("new SimpleDateFormat  = " + (System.nanoTime() - t0) + " ns");

        t0 = System.nanoTime();
        SYMS = DateFormatSymbols.getInstance(US);
        System.out.println("DateFormatSymbols.get = " + (System.nanoTime() - t0) + " ns");

        t0 = System.nanoTime();
        CAL = new GregorianCalendar(TimeZone.getDefault(), US);
        System.out.println("new GregorianCalendar = " + (System.nanoTime() - t0) + " ns");

        DEC = (DecimalFormat) NumberFormat.getIntegerInstance(US);
        DEC.setMinimumIntegerDigits(2);
        DEC.setGroupingUsed(false);
        DATE.setTime(1_700_000_000_000L);
        CAL.setTime(DATE);
        refSink = SDF.format(DATE);
        System.out.println("sample                = " + refSink);
        System.out.println();

        System.out.printf("%-26s", "stage (ns/op by block)");
        for (int b = 0; b < blocks; b++) {
            System.out.printf("%10d", b);
        }
        System.out.println();

        String[] names = {"dateSetTime", "calendarGetOnly", "calendarSetTimeAndGet",
                "decimalFormatInt", "stringBufferAppend", "symbolsShortMonths",
                "timeZoneGetDefault", "sdfFormatIntoBuffer", "sdfFormat", "stringFormat"};
        for (int i = 0; i < names.length; i++) {
            row(names[i], blocks, bs, i);
        }
        System.out.println("sink=" + sink);
    }
}
