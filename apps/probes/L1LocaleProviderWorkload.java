import java.text.DateFormat;
import java.text.DateFormatSymbols;
import java.text.DecimalFormatSymbols;
import java.text.NumberFormat;
import java.text.SimpleDateFormat;
import java.time.LocalDate;
import java.time.LocalDateTime;
import java.time.ZoneId;
import java.time.format.DateTimeFormatter;
import java.time.format.FormatStyle;
import java.time.format.TextStyle;
import java.time.temporal.ChronoField;
import java.util.Calendar;
import java.util.Currency;
import java.util.Date;
import java.util.GregorianCalendar;
import java.util.Locale;
import java.util.Map;
import java.util.TimeZone;
import java.util.TreeMap;
import sun.util.locale.provider.LocaleProviderAdapter;

/** L1 section 10 item 6 -- THE WORKLOAD THE SIX VACUOUS ROWS ASKED FOR.
 *
 *  Wave 5's bisection of the locale/calendar prefixes ended with one
 *  non-vacuous `+0` (`sun/util/calendar/ZoneInfoFile`, 3,792 door engagements,
 *  now retired) and SIX rows that read `+0` with `reached == 0`:
 *
 *    java/util/Date
 *    sun/util/locale/provider/CalendarDataUtility
 *    sun/util/locale/provider/JRELocaleProviderAdapter
 *    sun/util/locale/provider/LocaleResources
 *    sun/util/resources/Bundles
 *    sun/util/resources/LocaleData
 *
 *  A `reached == 0` row is not a green. It is the dial reporting that nothing
 *  in the 142-probe tree called through those classes' doors while it was
 *  armed, so the `+0` beside it is arithmetic on an empty set. Section 7 of
 *  the lane page is two vacuous greens that a sweep would have shipped.
 *
 *  This probe exists to make the next arming mean something. Every row drives
 *  a public API whose real-JDK implementation goes through one of the six:
 *
 *    Date                      -- constructed, compared, converted, printed
 *    LocaleResources           -- every date/time pattern, every symbol set,
 *                                 currency and locale display names
 *    JRELocaleProviderAdapter  -- asked for its providers directly
 *    CalendarDataUtility       -- Calendar.getDisplayName(s), first day of
 *                                 week, minimal days in first week
 *    LocaleData / Bundles      -- the class-based bundles behind
 *                                 DateFormatSymbols / DecimalFormatSymbols /
 *                                 Currency display names
 *
 *  DETERMINISM. Nothing here reads the clock or the host's zone: the default
 *  locale and zone are pinned in `main` before the first row, and every
 *  instant is a literal epoch millisecond. A probe in this family that prints
 *  `new Date()` is a probe that differs against itself.
 *
 *  Needs `--add-exports java.base/sun.util.locale.provider=ALL-UNNAMED` at
 *  COMPILE and at RUN (HotSpot only -- this VM does not enforce it, which is
 *  exactly how four probes read as differing in wave 5).
 */
public final class L1LocaleProviderWorkload {
    static int rows = 0;

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String tag, Body b) {
        rows++;
        String v;
        try {
            Object o = b.run();
            v = o == null ? "null" : String.valueOf(o);
        } catch (Throwable t) {
            String m = t.getMessage();
            v = "THREW " + t.getClass().getName() + (m == null ? "" : " msg=" + scrub(m));
        }
        System.out.println(tag + " |" + ascii(v) + "|");
    }

    static String scrub(String m) {
        return m.replaceAll("@[0-9a-f]+", "@X");
    }

    /** Print every non-ASCII character as its code unit.
     *
     *  Half these rows ARE non-ASCII -- currency symbols, German month names,
     *  Japanese era text -- and stdout encoding is a property of the process,
     *  not of the answer. Two VMs that disagree only about `file.encoding`
     *  would otherwise differ on every one of those rows and agree on none of
     *  the ones that matter. Escaped, the row carries the same information and
     *  the encoding drops out of the comparison.
     */
    static String ascii(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 128) {
                b.append(c);
            } else {
                b.append(String.format("\\u%04x", (int) c));
            }
        }
        return b.toString();
    }

    /** 2008-12-18T12:34:56.789Z -- the RFC-1123 date the Spring HttpHeaders
     *  regression was about, so a broken `EEE`/`MMM` shows up here too. */
    static final long T = 1229603696789L;
    static final long T2 = 0L;

    static Date d() {
        return new Date(T);
    }

    static void dateRows() {
        row("D.getTime", () -> d().getTime());
        row("D.toString", () -> d().toString());
        row("D.toInstant", () -> d().toInstant().toString());
        row("D.fromInstant", () -> Date.from(java.time.Instant.ofEpochMilli(T)).getTime());
        row("D.before", () -> new Date(T2).before(d()));
        row("D.after", () -> d().after(new Date(T2)));
        row("D.compareTo", () -> Integer.signum(d().compareTo(new Date(T2))));
        row("D.equals", () -> d().equals(new Date(T)));
        row("D.hashCode", () -> d().hashCode());
        row("D.clone", () -> ((Date) d().clone()).getTime());
        row("D.setTime", () -> {
            Date x = d();
            x.setTime(T2);
            return x.getTime();
        });
        row("D.toGMTString", () -> {
            @SuppressWarnings("deprecation")
            String s = d().toGMTString();
            return s;
        });
        row("D.toLocaleString", () -> {
            @SuppressWarnings("deprecation")
            String s = d().toLocaleString();
            return s;
        });
        // The deprecated accessors are the half of `java.util.Date` that goes
        // through `sun.util.calendar` rather than through `getTime`.
        row("D.getYear", () -> {
            @SuppressWarnings("deprecation")
            int y = d().getYear();
            return y;
        });
        row("D.getMonth", () -> {
            @SuppressWarnings("deprecation")
            int x = d().getMonth();
            return x;
        });
        row("D.getDate", () -> {
            @SuppressWarnings("deprecation")
            int x = d().getDate();
            return x;
        });
        row("D.getDay", () -> {
            @SuppressWarnings("deprecation")
            int x = d().getDay();
            return x;
        });
        row("D.getHours", () -> {
            @SuppressWarnings("deprecation")
            int x = d().getHours();
            return x;
        });
        row("D.getTimezoneOffset", () -> {
            @SuppressWarnings("deprecation")
            int x = d().getTimezoneOffset();
            return x;
        });
        row("D.parse", () -> {
            @SuppressWarnings("deprecation")
            long x = Date.parse("Thu, 18 Dec 2008 12:34:56 GMT");
            return x;
        });
        row("D.UTC", () -> {
            @SuppressWarnings("deprecation")
            long x = Date.UTC(108, 11, 18, 12, 34, 56);
            return x;
        });
    }

    /** Every one of these ends in `LocaleResources`, and most of them in
     *  `LocaleData` / `Bundles` under it. */
    static void patternRows() {
        for (Locale loc : new Locale[] {Locale.US, Locale.GERMANY, Locale.FRANCE, Locale.JAPAN}) {
            String tag = loc.toLanguageTag();
            row("R.dateFull." + tag, () ->
                    DateFormat.getDateInstance(DateFormat.FULL, loc).format(d()));
            row("R.dateShort." + tag, () ->
                    DateFormat.getDateInstance(DateFormat.SHORT, loc).format(d()));
            row("R.timeMedium." + tag, () ->
                    DateFormat.getTimeInstance(DateFormat.MEDIUM, loc).format(d()));
            row("R.dateTime." + tag, () -> DateFormat
                    .getDateTimeInstance(DateFormat.MEDIUM, DateFormat.SHORT, loc)
                    .format(d()));
            row("R.simple." + tag, () ->
                    new SimpleDateFormat("EEE, dd MMM yyyy HH:mm:ss zzz", loc).format(d()));
            row("R.number." + tag, () -> NumberFormat.getNumberInstance(loc).format(-12345.678));
            row("R.percent." + tag, () -> NumberFormat.getPercentInstance(loc).format(0.4567));
            row("R.currency." + tag, () -> NumberFormat.getCurrencyInstance(loc).format(1234.5));
            row("R.integer." + tag, () -> NumberFormat.getIntegerInstance(loc).format(9876543));
        }
    }

    static void symbolRows() {
        for (Locale loc : new Locale[] {Locale.US, Locale.GERMANY, Locale.JAPAN}) {
            String tag = loc.toLanguageTag();
            row("S.months." + tag, () -> String.join(",", new DateFormatSymbols(loc).getMonths()));
            row("S.shortMonths." + tag, () ->
                    String.join(",", new DateFormatSymbols(loc).getShortMonths()));
            row("S.weekdays." + tag, () ->
                    String.join(",", new DateFormatSymbols(loc).getWeekdays()));
            row("S.amPm." + tag, () -> String.join(",", new DateFormatSymbols(loc).getAmPmStrings()));
            row("S.eras." + tag, () -> String.join(",", new DateFormatSymbols(loc).getEras()));
            row("S.decimalSep." + tag, () ->
                    (int) new DecimalFormatSymbols(loc).getDecimalSeparator());
            row("S.groupSep." + tag, () ->
                    (int) new DecimalFormatSymbols(loc).getGroupingSeparator());
            row("S.currencySymbol." + tag, () -> new DecimalFormatSymbols(loc).getCurrencySymbol());
            row("S.intlCurrency." + tag, () ->
                    new DecimalFormatSymbols(loc).getInternationalCurrencySymbol());
        }
    }

    static void currencyAndNameRows() {
        for (String code : new String[] {"USD", "EUR", "JPY", "GBP"}) {
            row("C.symbol.us." + code, () -> Currency.getInstance(code).getSymbol(Locale.US));
            row("C.symbol.de." + code, () -> Currency.getInstance(code).getSymbol(Locale.GERMANY));
            row("C.display.us." + code, () ->
                    Currency.getInstance(code).getDisplayName(Locale.US));
            row("C.fractionDigits." + code, () ->
                    Currency.getInstance(code).getDefaultFractionDigits());
        }
        row("C.currencyOfLocale", () -> Currency.getInstance(Locale.GERMANY).getCurrencyCode());
        row("N.displayLanguage", () -> Locale.GERMANY.getDisplayLanguage(Locale.US));
        row("N.displayCountry", () -> Locale.GERMANY.getDisplayCountry(Locale.US));
        row("N.displayName", () -> Locale.JAPAN.getDisplayName(Locale.US));
        row("N.displayVariant", () -> Locale.forLanguageTag("ca-ES-VALENCIA")
                .getDisplayVariant(Locale.US));
        row("N.displayScript", () -> Locale.forLanguageTag("sr-Latn-RS")
                .getDisplayScript(Locale.US));
        row("N.tzDisplayLong", () ->
                TimeZone.getTimeZone("America/Sao_Paulo").getDisplayName(false, TimeZone.LONG,
                        Locale.US));
        row("N.tzDisplayShort", () ->
                TimeZone.getTimeZone("Europe/Berlin").getDisplayName(true, TimeZone.SHORT,
                        Locale.US));
    }

    /** `Calendar.getDisplayName(s)` and the week rules are
     *  `CalendarDataUtility`'s whole public surface. */
    static void calendarDataRows() {
        row("K.firstDayOfWeek.us", () -> Calendar.getInstance(Locale.US).getFirstDayOfWeek());
        row("K.firstDayOfWeek.de", () -> Calendar.getInstance(Locale.GERMANY).getFirstDayOfWeek());
        row("K.minimalDays.us", () ->
                Calendar.getInstance(Locale.US).getMinimalDaysInFirstWeek());
        row("K.minimalDays.de", () ->
                Calendar.getInstance(Locale.GERMANY).getMinimalDaysInFirstWeek());
        row("K.monthLong.us", () -> cal().getDisplayName(Calendar.MONTH, Calendar.LONG, Locale.US));
        row("K.monthShort.de", () ->
                cal().getDisplayName(Calendar.MONTH, Calendar.SHORT, Locale.GERMANY));
        row("K.dowLong.us", () ->
                cal().getDisplayName(Calendar.DAY_OF_WEEK, Calendar.LONG, Locale.US));
        row("K.amPm.us", () -> cal().getDisplayName(Calendar.AM_PM, Calendar.LONG, Locale.US));
        row("K.era.us", () -> cal().getDisplayName(Calendar.ERA, Calendar.LONG, Locale.US));
        row("K.displayNames.us", () -> {
            Map<String, Integer> m = cal().getDisplayNames(Calendar.MONTH, Calendar.SHORT,
                    Locale.US);
            return m == null ? "null" : new TreeMap<>(m).toString();
        });
        row("K.displayNamesStandalone.de", () -> {
            Map<String, Integer> m = cal().getDisplayNames(Calendar.DAY_OF_WEEK,
                    Calendar.LONG_STANDALONE, Locale.GERMANY);
            return m == null ? "null" : new TreeMap<>(m).toString();
        });
        row("K.calendarType", () -> cal().getCalendarType());
        row("K.weekYear", () -> cal().getWeekYear());
        row("K.weeksInWeekYear", () -> cal().getWeeksInWeekYear());
    }

    static Calendar cal() {
        Calendar c = new GregorianCalendar(TimeZone.getTimeZone("UTC"), Locale.US);
        c.setTimeInMillis(T);
        return c;
    }

    /** java.time's localized formatting reads the same bundles by a different
     *  road (`DateTimeTextProvider` -> `CalendarDataUtility`), so a repair on
     *  one road and not the other shows up as a split here. */
    static void javaTimeRows() {
        LocalDateTime ldt = LocalDateTime.ofInstant(java.time.Instant.ofEpochMilli(T),
                ZoneId.of("UTC"));
        row("J.isoDate", () -> ldt.toLocalDate().toString());
        row("J.rfc1123", () -> DateTimeFormatter.RFC_1123_DATE_TIME
                .format(ldt.atZone(ZoneId.of("GMT"))));
        row("J.styleFull", () -> DateTimeFormatter.ofLocalizedDate(FormatStyle.FULL)
                .withLocale(Locale.US).format(ldt));
        row("J.styleMedium.de", () -> DateTimeFormatter.ofLocalizedDate(FormatStyle.MEDIUM)
                .withLocale(Locale.GERMANY).format(ldt));
        row("J.styleDateTime", () -> DateTimeFormatter
                .ofLocalizedDateTime(FormatStyle.MEDIUM, FormatStyle.SHORT)
                .withLocale(Locale.US).format(ldt));
        row("J.monthText", () -> ldt.getMonth().getDisplayName(TextStyle.FULL, Locale.US));
        row("J.monthTextDe", () -> ldt.getMonth().getDisplayName(TextStyle.SHORT, Locale.GERMANY));
        row("J.dowText", () -> ldt.getDayOfWeek().getDisplayName(TextStyle.FULL, Locale.US));
        row("J.fieldText", () -> DateTimeFormatter.ofPattern("EEEE d MMMM yyyy G a", Locale.US)
                .format(ldt));
        row("J.parseBack", () -> LocalDate.parse("2008-12-18").get(ChronoField.DAY_OF_YEAR));
        row("J.zoneDisplay", () -> ZoneId.of("Europe/Berlin")
                .getDisplayName(TextStyle.FULL, Locale.US));
    }

    /** The adapter itself, asked directly rather than through a factory. */
    static void adapterRows() {
        row("A.forJRE", () -> LocaleProviderAdapter.forJRE().getClass().getName());
        row("A.adapterType", () -> LocaleProviderAdapter.forJRE().getAdapterType().toString());
        row("A.dateFormatProvider", () ->
                LocaleProviderAdapter.forJRE().getDateFormatProvider().getClass().getName());
        row("A.dateFormatSymbolsProvider", () -> LocaleProviderAdapter.forJRE()
                .getDateFormatSymbolsProvider().getClass().getName());
        row("A.decimalFormatSymbolsProvider", () -> LocaleProviderAdapter.forJRE()
                .getDecimalFormatSymbolsProvider().getClass().getName());
        row("A.numberFormatProvider", () ->
                LocaleProviderAdapter.forJRE().getNumberFormatProvider().getClass().getName());
        row("A.currencyNameProvider", () ->
                LocaleProviderAdapter.forJRE().getCurrencyNameProvider().getClass().getName());
        row("A.localeNameProvider", () ->
                LocaleProviderAdapter.forJRE().getLocaleNameProvider().getClass().getName());
        row("A.timeZoneNameProvider", () ->
                LocaleProviderAdapter.forJRE().getTimeZoneNameProvider().getClass().getName());
        row("A.collatorProvider", () ->
                LocaleProviderAdapter.forJRE().getCollatorProvider().getClass().getName());
        row("A.resourceBundleBased", () ->
                LocaleProviderAdapter.getResourceBundleBased().getClass().getName());
        row("A.adapterPreference", () -> LocaleProviderAdapter.getAdapterPreference().toString());
        row("A.availableLocalesNonEmpty", () -> LocaleProviderAdapter.forJRE()
                .getDateFormatProvider().getAvailableLocales().length > 0);
    }

    public static void main(String[] args) {
        // Pin both before the first row: every value below is otherwise a
        // function of the host.
        Locale.setDefault(Locale.US);
        TimeZone.setDefault(TimeZone.getTimeZone("UTC"));

        dateRows();
        patternRows();
        symbolRows();
        currencyAndNameRows();
        calendarDataRows();
        javaTimeRows();
        adapterRows();

        System.out.println("rows=" + rows);
    }
}
