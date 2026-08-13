import java.text.DateFormat;
import java.text.DateFormatSymbols;
import java.text.NumberFormat;
import java.util.Arrays;
import java.util.Currency;
import java.util.Date;
import java.util.Locale;

/**
 * W7-67: prove whether the VM reports the HOST default locale or a hardcoded one.
 * W7-80: prove whether the locale DATA behind that report is the host locale's.
 *
 * Deliberately NOT written as `assert Locale.getDefault() != null` or
 * `assertEquals("en_US", ...)` — the second passes *because of* the defect.
 * Every line here is an observable that differs between an en_US host and a
 * ru_RU host, so a diff against HotSpot on the same host is the whole test.
 *
 * Two vacuous shapes this file is written against, both live hazards here:
 *
 *  * `assertNotNull(dateFormat.format(d))` passes against en data on a ru host
 *    — a formatted date is non-empty whatever language it came out in. So the
 *    date rows below print the RENDERED TEXT, never a liveness flag.
 *  * `assertEquals("ru_RU", Locale.getDefault().toString())` tests W7-67's
 *    reporting fix, which already landed, and says nothing about the data. So
 *    the per-locale table below drives every row from an EXPLICIT `Locale`
 *    argument, independent of what the default happens to be.
 *
 * The `shortMonths[N]` row is the one `RJdkLogging` turns on: `SimpleFormatter`'s
 * default pattern opens with `%1$tb`, which `java.util.Formatter` renders from
 * `DateFormatSymbols.getInstance(l).getShortMonths()`. en "Aug" is 3 chars and
 * ru "авг." is 4, which is the whole `streamBytes` 175-vs-177 gap (two records).
 */
public final class DefaultLocaleProbe {
    public static void main(String[] args) {
        Locale def = Locale.getDefault();
        Locale fmt = Locale.getDefault(Locale.Category.FORMAT);
        Locale dsp = Locale.getDefault(Locale.Category.DISPLAY);

        System.out.println("default=" + def);
        System.out.println("format=" + fmt);
        System.out.println("display=" + dsp);
        System.out.println("default.tag=" + def.toLanguageTag());
        System.out.println("categories.collapsed=" + (def.equals(fmt) && def.equals(dsp)));

        // ---- the user.* properties that seed the above ----
        for (String k : new String[] {
            "user.language", "user.country", "user.variant", "user.script",
            "user.language.format", "user.country.format",
            "user.language.display", "user.country.display",
        }) {
            System.out.println(k + "=" + String.valueOf(System.getProperty(k)));
        }

        // ---- locale-sensitive observables ----
        // A fixed instant so the only variable is the locale.
        Date epoch = new Date(0L);
        System.out.println("dateFormat.FULL=" + DateFormat
            .getDateInstance(DateFormat.FULL, Locale.getDefault(Locale.Category.FORMAT))
            .format(epoch));
        System.out.println("months[0]=" + new DateFormatSymbols().getMonths()[0]);
        System.out.println("weekdays[1]=" + new DateFormatSymbols().getWeekdays()[1]);

        System.out.println("format.grouped=" + String.format("%,.2f", 1234.5d));
        System.out.println("number=" + NumberFormat.getNumberInstance().format(1234.5d));
        System.out.println("currency=" + NumberFormat.getCurrencyInstance().format(1234.5d));
        System.out.println("percent=" + NumberFormat.getPercentInstance().format(0.755d));
        System.out.println("currency.code="
            + NumberFormat.getCurrencyInstance().getCurrency().getCurrencyCode());

        // ---- the dotted-I trap: locale-sensitive case mapping ----
        System.out.println("i.toUpperCase=" + "i".toUpperCase());
        System.out.println("I.toLowerCase=" + "I".toLowerCase());
        System.out.println("i.toUpperCase(TR)=" + "i".toUpperCase(new Locale("tr", "TR")));
        System.out.println("title.toUpperCase=" + "title".toUpperCase());

        // ---- display names: these are what DISPLAY category governs ----
        System.out.println("displayLanguage.of.de="
            + Locale.GERMAN.getDisplayLanguage());
        System.out.println("displayCountry.self=" + def.getDisplayCountry());

        // ---- W7-80: the DATA behind the report, per locale ----
        // Every row here takes an explicit Locale, so it measures the locale
        // TABLES and not `Locale.getDefault()`. Run the same binary under
        // -Duser.language=xx -Duser.country=YY as well: the default-driven rows
        // above and the explicit rows here must tell the same story, and a VM
        // that reports ru while rendering en is exactly the disagreement.
        System.out.println("---- per-locale data ----");
        for (Locale l : new Locale[] {
            Locale.US, new Locale("ru", "RU"), Locale.GERMANY,
            new Locale("tr", "TR"), Locale.JAPAN, Locale.FRANCE,
        }) {
            row(l);
        }
        System.out.println("---- default-locale data ----");
        row(Locale.getDefault(Locale.Category.FORMAT));
    }

    /**
     * One locale, every surface stage 2 moves. Printed as `<tag>.<key>=<value>`
     * so a HotSpot/CratonVM diff is line-oriented.
     */
    private static void row(Locale l) {
        String t = l.toString();
        DateFormatSymbols s = DateFormatSymbols.getInstance(l);
        System.out.println(t + ".months=" + Arrays.toString(s.getMonths()));
        System.out.println(t + ".shortMonths=" + Arrays.toString(s.getShortMonths()));
        System.out.println(t + ".weekdays=" + Arrays.toString(s.getWeekdays()));
        System.out.println(t + ".shortWeekdays=" + Arrays.toString(s.getShortWeekdays()));
        System.out.println(t + ".eras=" + Arrays.toString(s.getEras()));
        System.out.println(t + ".amPm=" + Arrays.toString(s.getAmPmStrings()));

        // A FIXED instant: the only variable across VMs is the locale data.
        // 2026-08-12T06:00:00Z lands in the month whose abbreviation
        // RJdkLogging's `%1$tb` renders, so the shape below is the shape that
        // vector's transcript carries.
        Date fixed = new Date(1786_000_000_000L);
        System.out.println(t + ".date.FULL="
            + DateFormat.getDateInstance(DateFormat.FULL, l).format(fixed));
        System.out.println(t + ".date.MEDIUM="
            + DateFormat.getDateInstance(DateFormat.MEDIUM, l).format(fixed));
        // The literal SimpleFormatter head: `java.util.logging.SimpleFormatter`'s
        // default format is
        // "%1$tb %1$td, %1$tY %1$tl:%1$tM:%1$tS %1$Tp %2$s%n%4$s: %5$s%6$s%n".
        // Its first conversion is the one that moves; the whole prefix is
        // printed so a change anywhere in it is visible.
        System.out.println(t + ".simpleFormatterHead=["
            + String.format(l, "%1$tb %1$td, %1$tY %1$tl:%1$tM:%1$tS %1$Tp", fixed) + "]");
        System.out.println(t + ".tb.len="
            + String.format(l, "%1$tb", fixed).length());

        System.out.println(t + ".number=" + NumberFormat.getNumberInstance(l).format(1234.5d));
        System.out.println(t + ".currency=" + NumberFormat.getCurrencyInstance(l).format(1234.5d));
        System.out.println(t + ".currencyNeg="
            + NumberFormat.getCurrencyInstance(l).format(-1234.5d));
        System.out.println(t + ".percent=" + NumberFormat.getPercentInstance(l).format(0.755d));
        System.out.println(t + ".grouped=" + String.format(l, "%,.2f", 1234.5d));

        // Symbol AND code: they come from different tables and only the code
        // is derivable from the country, so a VM can get one right and the
        // other wrong. `getCurrency()` on the format instance is what
        // `NumberFormat` actually rendered with.
        Currency cur = NumberFormat.getCurrencyInstance(l).getCurrency();
        System.out.println(t + ".currency.code=" + (cur == null ? "null" : cur.getCurrencyCode()));
        System.out.println(t + ".currency.symbol="
            + (cur == null ? "null" : cur.getSymbol(l)));
        try {
            Currency byLocale = Currency.getInstance(l);
            System.out.println(t + ".currency.ofLocale="
                + (byLocale == null ? "null" : byLocale.getCurrencyCode()));
        } catch (RuntimeException e) {
            System.out.println(t + ".currency.ofLocale=EX " + e.getClass().getName());
        }

        System.out.println(t + ".displayLanguage.self=" + l.getDisplayLanguage(l));
        System.out.println(t + ".displayCountry.self=" + l.getDisplayCountry(l));
    }
}
