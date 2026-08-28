import java.util.TimeZone;

/**
 * Census of the date a `%t` conversion renders, over the inputs the
 * epoch-day → (year, month, day) conversion used to get wrong.
 *
 * Written for
 * `stringformat-tb-month-conversion-off-by-one-panics-native-20260828`: the
 * Tomcat `TestOneLineFormatterPerformance` failure was
 * `native method panic: index out of bounds: the len is 12 but the index is 12`
 * out of `String.format`, and the trigger turned out to be January 1 of any
 * leap year — with every OTHER day of a leap year silently rendering as the day
 * before. The failing test formats `System.nanoTime()` as though it were epoch
 * millis, so it sweeps arbitrary far-future dates and eventually lands on one,
 * which is why it looked nondeterministic and why probes built from realistic
 * timestamps never reproduced it.
 *
 * Run on HotSpot and on CratonVM and diff. Every line is
 * `label epochMillis -> rendering`, so a diff names the input.
 *
 * Forces UTC, because `%t` renders LOCAL fields and the point of comparison is
 * the calendar arithmetic, not the two runs' default zones.
 */
public final class FormatTemporalLeapCensus {

    /** The exact format string the failing Tomcat test uses. */
    static final String TOMCAT_FMT = "%1$td-%1$tb-%1$tY %1$tH:%1$tM:%1$tS";

    static final long DAY = 86400000L;

    static void row(StringBuilder out, String label, long millis, String rendered) {
        out.append(label).append(' ').append(millis).append(" -> ").append(rendered).append('\n');
    }

    static void census(StringBuilder out, String label, long millis) {
        row(out, label, millis, String.format(TOMCAT_FMT, Long.valueOf(millis)));
        // Every field the conversion can answer about the DATE, one per line, so
        // a diff says which one drifted rather than just "the string differs".
        row(out, label + ".F", millis, String.format("%tF", Long.valueOf(millis)));
        row(out, label + ".j", millis, String.format("%tj", Long.valueOf(millis)));
        row(out, label + ".B", millis, String.format("%tB", Long.valueOf(millis)));
        row(out, label + ".b", millis, String.format("%tb", Long.valueOf(millis)));
        row(out, label + ".A", millis, String.format("%tA", Long.valueOf(millis)));
        row(out, label + ".a", millis, String.format("%ta", Long.valueOf(millis)));
        row(out, label + ".c", millis, String.format("%tc", Long.valueOf(millis)));
        row(out, label + ".D", millis, String.format("%tD", Long.valueOf(millis)));
    }

    public static void main(String[] args) {
        TimeZone.setDefault(TimeZone.getTimeZone("UTC"));
        StringBuilder out = new StringBuilder();

        // 1. January 1 of every leap year from 1904 to 2100 — the day the
        //    native panicked on. Plus Dec 31 and Feb 29 of the same years,
        //    which rendered as the day before.
        for (int year = 1904; year <= 2100; year++) {
            if (!((year % 4 == 0 && year % 100 != 0) || year % 400 == 0)) {
                continue;
            }
            long jan1 = utcMillis(year, 1, 1);
            census(out, "leapJan1." + year, jan1);
            census(out, "leapFeb29." + year, utcMillis(year, 2, 29));
            census(out, "leapDec31." + year, utcMillis(year, 12, 31));
            census(out, "leapJan2." + year, jan1 + DAY);
            census(out, "preLeapDec31." + (year - 1), jan1 - DAY);
        }

        // 2. A contiguous run across a leap boundary, one day at a time, so an
        //    off-by-one anywhere in the year shows as a run of differing lines
        //    rather than a single row.
        long cursor = utcMillis(2023, 12, 20);
        for (int i = 0; i < 90; i++) {
            row(out, "walk2024." + i, cursor,
                    String.format("%tF %tA", Long.valueOf(cursor), Long.valueOf(cursor)));
            cursor += DAY;
        }

        // 3. The domain the failing test actually samples: `System.nanoTime()`
        //    read as epoch millis, which lands ~8-9 millennia out. A few fixed
        //    points there, including one deliberately on a leap January 1.
        for (long millis : new long[] {
                277_000_000_000_000L,
                277_000_086_400_000L,
                300_123_456_789_012L,
                utcMillis(10752, 1, 1),
                utcMillis(10752, 2, 29),
                utcMillis(9996, 1, 1),
        }) {
            census(out, "farFuture", millis);
        }

        // 4. Negative epoch millis — before 1970.
        for (long millis : new long[] {
                -1L,
                -DAY,
                utcMillis(1969, 12, 31),
                utcMillis(1900, 1, 1),
                utcMillis(1600, 2, 29),
                utcMillis(1583, 1, 1),
        }) {
            census(out, "past", millis);
        }

        // 5. BEFORE THE GREGORIAN CUTOVER, where the two runtimes are expected
        //    to differ and a diff here is NOT this bug. HotSpot's `Formatter`
        //    goes through `GregorianCalendar`, which switches to the JULIAN
        //    calendar before 1582-10-15; CratonVM is proleptic Gregorian
        //    throughout, so the same instant is two days apart at year 1.
        //    Measured identical on CratonVM before and after the leap-year fix
        //    (`0001-01-01` both times, against HotSpot's `0001-01-03`), so it is
        //    a separate, pre-existing modelling difference. Kept in the census,
        //    labelled, so nobody re-diagnoses it as a regression.
        for (long millis : new long[] {
                utcMillis(1582, 10, 4),
                utcMillis(1, 1, 1),
        }) {
            census(out, "preGregorianCutover.EXPECTED-DIFF", millis);
        }

        System.out.print(out);
        System.out.println("rows: " + out.toString().split("\n").length);
    }

    /** Epoch millis for a UTC midnight, computed without java.time or Calendar. */
    static long utcMillis(int year, int month, int day) {
        long y = year - (month <= 2 ? 1 : 0);
        long era = (y >= 0 ? y : y - 399) / 400;
        long yoe = y - era * 400;
        long doy = (153 * (month + (month > 2 ? -3 : 9)) + 2) / 5 + day - 1;
        long doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        return (era * 146097 + doe - 719468) * DAY;
    }
}
