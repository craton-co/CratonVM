import java.util.Calendar;
import java.util.GregorianCalendar;
import java.util.TimeZone;

/** DST-awareness probe for the synthetic TimeZone table (HIB-CV-34 family). */
public class TzDstProbe {
    static int bad = 0;

    static void check(String what, long got, long want) {
        if (got != want) {
            bad++;
            System.out.println("BAD " + what + " got=" + got + " want=" + want);
        }
    }

    public static void main(String[] args) {
        TimeZone paris = TimeZone.getTimeZone("Europe/Paris");
        // Winter (standard, +1h) and summer (DST, +2h) instants.
        long jan15_2018 = 1516000000000L; // 2018-01-15
        long jul15_2018 = 1531650000000L; // 2018-07-15
        check("paris.raw", paris.getRawOffset(), 3600000L);
        check("paris.jan", paris.getOffset(jan15_2018), 3600000L);
        check("paris.jul", paris.getOffset(jul15_2018), 7200000L);
        // DST-boundary day the Hibernate test failed on: 2018-10-28. At
        // 2018-10-28T00:30 UTC Paris is still CEST (+2h); at 01:30 UTC CET (+1h).
        long oct28_0030utc = 1540686600000L;
        long oct28_0130utc = 1540690200000L;
        check("paris.oct28-before", paris.getOffset(oct28_0030utc), 7200000L);
        check("paris.oct28-after", paris.getOffset(oct28_0130utc), 3600000L);

        TimeZone akl = TimeZone.getTimeZone("Pacific/Auckland");
        check("akl.raw", akl.getRawOffset(), 12 * 3600000L);
        check("akl.jan", akl.getOffset(jan15_2018), 13 * 3600000L); // NZDT
        check("akl.jul", akl.getOffset(jul15_2018), 12 * 3600000L); // NZST
        // NZ DST start 2018: Sep 30 02:00 NZST -> 03:00 NZDT (13:00 Sep 29 UTC…
        // actually 2018-09-29T14:00Z). Before/after instants:
        long sep29_1330utc = 1538227800000L; // 2018-09-29T13:30Z -> still NZST
        long sep29_1430utc = 1538231400000L; // 2018-09-29T14:30Z -> NZDT
        check("akl.sep30-before", akl.getOffset(sep29_1330utc), 12 * 3600000L);
        check("akl.sep30-after", akl.getOffset(sep29_1430utc), 13 * 3600000L);

        // Calendar round-trip in Paris across a summer date (what the JDBC
        // Calendar-bound bind path exercises).
        Calendar c = new GregorianCalendar(paris);
        c.clear();
        c.set(2018, Calendar.JULY, 15, 12, 0, 0);
        long millis = c.getTimeInMillis();
        Calendar c2 = new GregorianCalendar(paris);
        c2.setTimeInMillis(millis);
        check("cal.hour", c2.get(Calendar.HOUR_OF_DAY), 12);

        // Untouched zones keep working (regression guard for the fallback path).
        TimeZone utc = TimeZone.getTimeZone("UTC");
        check("utc.raw", utc.getRawOffset(), 0);
        TimeZone la = TimeZone.getTimeZone("America/Los_Angeles");
        check("la.raw", la.getRawOffset(), -8 * 3600000L);

        System.out.println(bad == 0 ? "@@PASS" : "@@FAIL bad=" + bad);
        System.out.println("@@DONE");
    }
}
