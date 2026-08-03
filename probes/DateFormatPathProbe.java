/*
 * DateFormatPathProbe — which branch of `SimpleDateFormat.subFormat` is being
 * taken, and what the pieces under it cost.
 *
 * `DateFormatCostProbe` shows `SimpleDateFormat.format(Date)` at ~557x HotSpot
 * while `String.format("%t...")` — which formats the same fields out of the
 * same Calendar — is within 1.6x. So this is not "date formatting is slow";
 * something on SimpleDateFormat's path specifically is.
 *
 * `subFormat` has two ways to turn a month number into "Nov":
 *
 *     if (useDateFormatSymbols()) {                 // cheap: array index
 *         String[] months = formatData.getShortMonths();
 *         current = months[value];
 *     } else {                                      // expensive: provider lookup
 *         current = calendar.getDisplayName(field, style, locale);
 *     }
 *
 * The expensive arm rebuilds a `DateFormatSymbols` per call. This probe reports
 * which arm each VM takes (by reading the private fields that decide it) and
 * times both arms independently, so the answer does not depend on inferring it
 * from a total.
 *
 *   <vm> -cp <out> DateFormatPathProbe [iters]
 */

import java.lang.reflect.Field;
import java.text.DateFormatSymbols;
import java.text.SimpleDateFormat;
import java.util.Calendar;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.Locale;
import java.util.TimeZone;

public class DateFormatPathProbe {

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;

        SimpleDateFormat sdf = new SimpleDateFormat("dd-MMM-yyyy HH:mm:ss", Locale.US);
        sdf.setTimeZone(TimeZone.getDefault());
        Date fixed = new Date(1_700_000_000_000L);
        sdf.format(fixed); // force lazy init

        // --- Which arm? -----------------------------------------------------
        report(sdf, "useDateFormatSymbols");
        report(sdf, "locale");
        Object cal = readField(sdf, "calendar");
        System.out.println("PROBE-FIELD calendar            = " +
                (cal == null ? "null" : cal.getClass().getName()));
        if (cal instanceof Calendar) {
            System.out.println("PROBE-FIELD calendar.getCalendarType() = " +
                    ((Calendar) cal).getCalendarType());
        }
        Object fd = readField(sdf, "formatData");
        System.out.println("PROBE-FIELD formatData          = " +
                (fd == null ? "null" : fd.getClass().getName()));

        // --- The two arms, priced separately ---------------------------------
        DateFormatSymbols symbols = DateFormatSymbols.getInstance(Locale.US);
        GregorianCalendar gc = new GregorianCalendar(TimeZone.getDefault(), Locale.US);
        gc.setTime(fixed);

        warm(symbols, gc, Math.min(iters / 10, 2000));

        long t0 = System.nanoTime();
        String s = null;
        for (int i = 0; i < iters; i++) {
            s = symbols.getShortMonths()[10];
        }
        row("CHEAP arm: DateFormatSymbols.getShortMonths()[m]", System.nanoTime() - t0, iters);

        t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            s = gc.getDisplayName(Calendar.MONTH, Calendar.SHORT, Locale.US);
        }
        row("EXPENSIVE arm: Calendar.getDisplayName(MONTH,SHORT)", System.nanoTime() - t0, iters);

        t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            s = DateFormatSymbols.getInstance(Locale.US).getShortMonths()[10];
        }
        row("  of which DateFormatSymbols.getInstance(Locale.US)", System.nanoTime() - t0, iters);

        // --- Pieces `String.format` shares, as a control ----------------------
        t0 = System.nanoTime();
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            gc.setTimeInMillis(1_700_000_000_000L + i);
            acc += gc.get(Calendar.MONTH);
        }
        row("CONTROL: Calendar.setTimeInMillis + get(MONTH)", System.nanoTime() - t0, iters);

        t0 = System.nanoTime();
        for (int i = 0; i < iters; i++) {
            s = sdf.format(fixed);
        }
        row("TOTAL: SimpleDateFormat.format(Date)", System.nanoTime() - t0, iters);

        System.out.println("PROBE-SINK " + s + " " + acc);
    }

    private static void warm(DateFormatSymbols symbols, GregorianCalendar gc, int n) {
        String s = null;
        for (int i = 0; i < n; i++) {
            s = symbols.getShortMonths()[10];
            s = gc.getDisplayName(Calendar.MONTH, Calendar.SHORT, Locale.US);
            s = DateFormatSymbols.getInstance(Locale.US).getShortMonths()[10];
        }
        if (s == null) {
            throw new IllegalStateException();
        }
    }

    private static Object readField(Object o, String name) {
        for (Class<?> c = o.getClass(); c != null; c = c.getSuperclass()) {
            try {
                Field f = c.getDeclaredField(name);
                f.setAccessible(true);
                return f.get(o);
            } catch (NoSuchFieldException e) {
                // keep walking
            } catch (Exception e) {
                return "<unreadable: " + e + ">";
            }
        }
        return "<no such field>";
    }

    private static void report(Object o, String name) {
        System.out.println(String.format(Locale.US, "PROBE-FIELD %-22s = %s", name, readField(o, name)));
    }

    private static void row(String name, long ns, int iters) {
        System.out.println(String.format(Locale.US, "PROBE %-52s %10.3f ms  %9.1f ns/op", name,
                ns / 1e6, (double) ns / iters));
    }
}
