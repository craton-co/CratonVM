import java.util.Calendar;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.Locale;
import java.util.TimeZone;

/**
 * Isolates `java.util.Calendar`'s cost, which the 30.A decomposition points at:
 * `SimpleDateFormat.format("ss")` — a single 2-digit numeric field, the
 * cheapest thing SimpleDateFormat can do — costs 51 us on CratonVM against
 * HotSpot's 59 ns, with NO compile failures anywhere on the path, and
 * `calendarSetTimeAndGet` alone measured 30 us.
 *
 * `Calendar.setTime` marks the fields dirty; the first `get` runs
 * `computeFields`, which asks the `TimeZone` for its offset at that instant and
 * then runs `sun.util.calendar.BaseCalendar` date arithmetic. Each of those is
 * timed separately here.
 *
 * All stages are static methods with the loop inline.
 */
public final class CalendarCostProbe {

    private static Calendar CAL;
    private static TimeZone TZ;
    private static final Date DATE = new Date(1_700_000_000_000L);
    private static long sink;

    /** setTimeInMillis only — marks fields dirty, computes nothing yet. */
    private static void setOnly(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CAL.setTimeInMillis(1_700_000_000_000L + i);
            s += i;
        }
        sink += s;
    }

    /** set + one get — the first get pays computeFields. */
    private static void setThenGet(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            CAL.setTimeInMillis(1_700_000_000_000L + i);
            s += CAL.get(Calendar.SECOND);
        }
        sink += s;
    }

    /** repeated get with no set — fields already computed, should be ~free. */
    private static void getOnly(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += CAL.get(Calendar.SECOND);
        }
        sink += s;
    }

    /** The timezone offset lookup computeFields performs. */
    private static void tzGetOffset(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += TZ.getOffset(1_700_000_000_000L + i);
        }
        sink += s;
    }

    private static void tzInDaylight(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            DATE.setTime(1_700_000_000_000L + i);
            s += TZ.inDaylightTime(DATE) ? 1 : 0;
        }
        sink += s;
    }

    private static void tzGetRawOffset(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += TZ.getRawOffset();
        }
        sink += s;
    }

    /** getTimeInMillis on a clean calendar — the reverse direction. */
    private static void getTimeInMillis(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += CAL.getTimeInMillis() & 1;
        }
        sink += s;
    }

    /** A UTC calendar: same arithmetic, but the zone has no rules to search. */
    private static Calendar UTC_CAL;

    private static void setThenGetUtc(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            UTC_CAL.setTimeInMillis(1_700_000_000_000L + i);
            s += UTC_CAL.get(Calendar.SECOND);
        }
        sink += s;
    }

    public static void main(String[] args) {
        int blocks = args.length > 0 ? Integer.parseInt(args[0]) : 3;
        int bs = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;

        TZ = TimeZone.getDefault();
        CAL = new GregorianCalendar(TZ, Locale.US);
        UTC_CAL = new GregorianCalendar(TimeZone.getTimeZone("UTC"), Locale.US);
        CAL.setTimeInMillis(1_700_000_000_000L);
        CAL.get(Calendar.SECOND);
        UTC_CAL.setTimeInMillis(1_700_000_000_000L);
        UTC_CAL.get(Calendar.SECOND);
        System.out.println("default zone = " + TZ.getID() + "  raw=" + TZ.getRawOffset());

        System.out.printf("%-24s", "stage (ns/op by block)");
        for (int b = 0; b < blocks; b++) {
            System.out.printf("%10d", b);
        }
        System.out.println();

        String[] names = {"setOnly", "getOnly", "setThenGet", "setThenGetUtc",
                "tzGetRawOffset", "tzGetOffset", "tzInDaylight", "getTimeInMillis"};
        for (int k = 0; k < names.length; k++) {
            StringBuilder out = new StringBuilder(String.format("%-24s", names[k]));
            for (int b = 0; b < blocks; b++) {
                long t0 = System.nanoTime();
                switch (k) {
                    case 0: setOnly(bs); break;
                    case 1: getOnly(bs); break;
                    case 2: setThenGet(bs); break;
                    case 3: setThenGetUtc(bs); break;
                    case 4: tzGetRawOffset(bs); break;
                    case 5: tzGetOffset(bs); break;
                    case 6: tzInDaylight(bs); break;
                    default: getTimeInMillis(bs); break;
                }
                out.append(String.format("%10d", (System.nanoTime() - t0) / bs));
            }
            System.out.println(out);
        }
        System.out.println("sink=" + sink);
    }
}
