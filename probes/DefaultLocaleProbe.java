import java.text.DateFormat;
import java.text.DateFormatSymbols;
import java.text.NumberFormat;
import java.util.Date;
import java.util.Locale;

/**
 * W7-67: prove whether the VM reports the HOST default locale or a hardcoded one.
 *
 * Deliberately NOT written as `assert Locale.getDefault() != null` or
 * `assertEquals("en_US", ...)` — the second passes *because of* the defect.
 * Every line here is an observable that differs between an en_US host and a
 * ru_RU host, so a diff against HotSpot on the same host is the whole test.
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
    }
}
