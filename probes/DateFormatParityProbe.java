/*
 * DateFormatParityProbe — byte-for-byte parity battery for the native
 * `DateFormat.format(Date)` fast path (`native-builtins/src/date_format_fast.rs`).
 *
 * Prints one line per (pattern, timezone, locale, instant) cell. Run it on
 * HotSpot and on CratonVM and `diff` the two outputs: any difference is a
 * faithfulness bug in the native, full stop. The matrix deliberately includes
 * the cases the fast path is supposed to DECLINE (zone names, week numbers,
 * standalone months, non-ASCII digits, pre-Gregorian dates, a moved cutover, a
 * SimpleDateFormat subclass), because a wrong decline is just as much a bug as
 * a wrong format — it would silently give up the speedup.
 *
 *   javac -d <out> probes/DateFormatParityProbe.java
 *   java -cp <out> DateFormatParityProbe > hs.txt
 *   <cratonvm> -cp <out> DateFormatParityProbe > cv.txt
 *   diff hs.txt cv.txt && echo PARITY-OK
 */

import java.text.DateFormatSymbols;
import java.text.FieldPosition;
import java.text.SimpleDateFormat;
import java.util.Calendar;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.Locale;
import java.util.TimeZone;

public class DateFormatParityProbe {

    private static final String[] PATTERNS = {
        // The two that matter in production: Tomcat's access log / JULI, and
        // the log4j-style stamp.
        "dd-MMM-yyyy HH:mm:ss",
        "yyyy-MM-dd HH:mm:ss.SSS",
        // Every supported letter, alone and repeated.
        "G", "GG",
        "y", "yy", "yyy", "yyyy", "yyyyy",
        "M", "MM", "MMM", "MMMM", "MMMMM",
        "d", "dd", "ddd",
        "D", "DDD",
        "H", "HH", "k", "kk", "h", "hh", "K", "KK",
        "m", "mm", "s", "ss", "S", "SS", "SSS", "SSSSS",
        "E", "EE", "EEE", "EEEE",
        "a",
        "Z", "X", "XX", "XXX",
        // Quoting.
        "'at' HH:mm", "HH''mm", "'''ticks'''", "yyyy'T'HH",
        // Mixed, and adjacent fields with no separator.
        "yyyyMMddHHmmssSSS", "EEE, d MMM yyyy HH:mm:ss Z",
        // Expected to DECLINE — zone names, week family, standalone month.
        "zzz", "zzzz", "w", "ww", "W", "F", "YYYY", "u", "LLL",
    };

    private static final String[] ZONES = {
        "UTC", "America/Los_Angeles", "Europe/Moscow", "Asia/Kolkata",
        "Australia/Lord_Howe", "America/St_Johns", "Pacific/Kiritimati",
    };

    private static final Locale[] LOCALES = {
        Locale.US, Locale.UK, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN,
        new Locale("ar", "EG"), // Arabic-Indic digits in some configurations
        new Locale("ru", "RU"), // standalone month forms
    };

    private static final long[] INSTANTS = {
        0L,                       // 1970-01-01T00:00:00Z, a Thursday
        -1L,                      // one millisecond before the epoch
        1_700_000_000_000L,       // 2023-11-14
        1_709_208_000_000L,       // 2024-02-29, a leap day
        1_710_054_000_000L,       // 2024-03-10, US DST spring-forward day
        1_730_610_000_000L,       // 2024-11-03, US DST fall-back day
        1_735_689_599_999L,       // 2024-12-31T23:59:59.999Z
        946_684_800_000L,         // 2000-01-01
        -2_208_988_800_000L,      // 1900-01-01
        -12_219_292_800_000L,     // the Gregorian cutover instant itself
        -12_219_292_800_001L,     // one ms before it — Julian rules
        -62_135_596_800_000L,     // year 1
        253_402_300_799_000L,     // year 9999
        4_102_444_800_000L,       // 2100-01-01, past most tzdb transition tables
    };

    public static void main(String[] args) throws Exception {
        TimeZone originalDefault = TimeZone.getDefault();
        try {
            for (String zoneId : ZONES) {
                TimeZone zone = TimeZone.getTimeZone(zoneId);
                TimeZone.setDefault(zone);
                for (Locale locale : LOCALES) {
                    for (String pattern : PATTERNS) {
                        SimpleDateFormat sdf;
                        try {
                            sdf = new SimpleDateFormat(pattern, locale);
                        } catch (RuntimeException e) {
                            emit(zoneId, locale, pattern, "-", "CTOR-THREW:" + e.getClass().getName());
                            continue;
                        }
                        sdf.setTimeZone(zone);
                        for (long instant : INSTANTS) {
                            emit(zoneId, locale, pattern, Long.toString(instant),
                                    formatOrError(sdf, new Date(instant)));
                        }
                        // The calendar must be left holding the last instant —
                        // the native skips the JDK's eager computeFields, so
                        // this checks the state it leaves behind is equivalent.
                        Calendar after = sdf.getCalendar();
                        emit(zoneId, locale, pattern, "calendar-after",
                                after.getTimeInMillis() + "/" + after.get(Calendar.YEAR) + "/" +
                                        after.get(Calendar.MONTH) + "/" + after.get(Calendar.DAY_OF_MONTH) +
                                        "/" + after.get(Calendar.HOUR_OF_DAY));
                    }
                }
            }

            // --- Shapes the fast path must decline, checked explicitly -------
            declineCases();
        } finally {
            TimeZone.setDefault(originalDefault);
        }
        System.out.println("PARITY-PROBE-END");
    }

    private static void declineCases() throws Exception {
        TimeZone utc = TimeZone.getTimeZone("UTC");
        Date d = new Date(1_700_000_000_000L);

        // A subclass overriding the virtual 3-arg format: `format(Date)` is
        // final and must still route through the override.
        SimpleDateFormat sub = new SimpleDateFormat("yyyy", Locale.US) {
            @Override
            public StringBuffer format(Date date, StringBuffer toAppendTo, FieldPosition pos) {
                return toAppendTo.append("SUBCLASS");
            }
        };
        sub.setTimeZone(utc);
        emit("UTC", Locale.US, "subclass-override", "-", sub.format(d));

        // Application-supplied symbols: `useDateFormatSymbols` flips true and
        // the month name must come from OUR symbols, not the locale's.
        SimpleDateFormat custom = new SimpleDateFormat("MMM yyyy", Locale.US);
        custom.setTimeZone(utc);
        DateFormatSymbols syms = new DateFormatSymbols(Locale.US);
        String[] shortMonths = syms.getShortMonths();
        shortMonths[10] = "XXX";
        syms.setShortMonths(shortMonths);
        custom.setDateFormatSymbols(syms);
        emit("UTC", Locale.US, "custom-symbols", "-", custom.format(d));

        // A moved Gregorian cutover changes which rules apply where.
        SimpleDateFormat movedCutover = new SimpleDateFormat("yyyy-MM-dd G", Locale.US);
        GregorianCalendar gc = new GregorianCalendar(utc, Locale.US);
        gc.setGregorianChange(new Date(0L)); // pure Julian before 1970
        movedCutover.setCalendar(gc);
        movedCutover.setTimeZone(utc);
        emit("UTC", Locale.US, "moved-cutover", "-", movedCutover.format(new Date(-3_000_000_000_000L)));

        // A non-Gregorian calendar.
        SimpleDateFormat nonGreg = new SimpleDateFormat("yyyy-MM-dd", Locale.US);
        nonGreg.setCalendar(Calendar.getInstance(utc, Locale.forLanguageTag("th-TH-u-ca-buddhist")));
        nonGreg.setTimeZone(utc);
        emit("UTC", Locale.US, "non-gregorian", "-", nonGreg.format(d));

        // Re-applying a pattern must invalidate any cached plan.
        SimpleDateFormat reapply = new SimpleDateFormat("yyyy", Locale.US);
        reapply.setTimeZone(utc);
        emit("UTC", Locale.US, "reapply", "before", reapply.format(d));
        reapply.applyPattern("MMM-dd");
        emit("UTC", Locale.US, "reapply", "after", reapply.format(d));
        reapply.applyPattern("zzz");
        emit("UTC", Locale.US, "reapply", "declining", reapply.format(d));
        reapply.applyPattern("yyyy");
        emit("UTC", Locale.US, "reapply", "back", reapply.format(d));

        // Null date: the JDK's own NullPointerException, from its own frame.
        SimpleDateFormat plain = new SimpleDateFormat("yyyy", Locale.US);
        plain.setTimeZone(utc);
        try {
            plain.format((Date) null);
            emit("UTC", Locale.US, "null-date", "-", "NO-THROW");
        } catch (Exception e) {
            emit("UTC", Locale.US, "null-date", "-", "THREW:" + e.getClass().getName());
        }

        // A Date mutated through the deprecated setters leaves `cdate` dirty,
        // so `fastTime` alone is not the answer.
        @SuppressWarnings("deprecation")
        Date mutated = new Date(1_700_000_000_000L);
        mutated.setYear(99);
        SimpleDateFormat mut = new SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.US);
        mut.setTimeZone(utc);
        emit("UTC", Locale.US, "dirty-cdate", "-", mut.format(mutated));
    }

    private static String formatOrError(SimpleDateFormat sdf, Date d) {
        try {
            return sdf.format(d);
        } catch (RuntimeException e) {
            return "THREW:" + e.getClass().getName();
        }
    }

    private static void emit(String zone, Locale locale, String pattern, String instant, String out) {
        // Tab-separated so a diff points straight at the offending cell.
        System.out.println(zone + '	' + locale + '	' + pattern + '	' + instant + '	' + out);
    }
}
