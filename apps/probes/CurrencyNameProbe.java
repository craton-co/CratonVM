import java.util.*;

/** L3 residual 6.4 — `Currency.getDisplayName` answers the CODE.
 *
 *  The record's hypothesis is "the CLDR bundle is not reachable through
 *  LocaleServiceProvider". This walks the chain the JDK actually walks, one
 *  rung at a time, so the first rung that differs from HotSpot names the
 *  defect instead of a guess about it. Every rung is a one-way question with
 *  its own row.
 */
public class CurrencyNameProbe {
    static int rows = 0;

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + v + "|");
    }

    static void t(String tag, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            p(tag + " THREW", e.getClass().getName() + ": " + e.getMessage());
        }
    }

    public static void main(String[] a) {
        Currency usd = Currency.getInstance("USD");
        p("code", usd.getCurrencyCode());
        p("symbol en", usd.getSymbol(Locale.ENGLISH));
        p("displayName en", usd.getDisplayName(Locale.ENGLISH));
        p("displayName default", usd.getDisplayName());
        p("numeric", usd.getNumericCode());
        p("fraction digits", usd.getDefaultFractionDigits());

        // The curated fallback this VM needs for an image without
        // `jdk.localedata`, measured rather than guessed.
        for (String c : new String[] {"GBP", "JPY", "CNY", "CHF", "CAD", "AUD", "SEK", "INR"}) {
            p("name " + c, Currency.getInstance(c).getDisplayName(Locale.ENGLISH));
        }

        Currency eur = Currency.getInstance("EUR");
        p("eur displayName en", eur.getDisplayName(Locale.ENGLISH));
        p("eur symbol en", eur.getSymbol(Locale.ENGLISH));

        // The resource bundle the JDK reads the display name out of.
        t("bundle CurrencyNames", () -> {
            ResourceBundle b = ResourceBundle.getBundle(
                    "sun.util.resources.CurrencyNames", Locale.ENGLISH);
            p("bundle class", b.getClass().getName());
            p("bundle USD", b.getString("USD"));
            // CLDR keys the SYMBOL under the uppercase code and the display
            // NAME under the lowercase one. If the lowercase key is present,
            // the data the JDK's provider reads is already reachable here.
            t("lower", () -> p("bundle usd", b.getString("usd")));
            t("lower eur", () -> p("bundle eur", b.getString("eur")));
        });

        // A bundle that is NOT currency-specific, to tell "no bundles at all"
        // apart from "this bundle in particular".
        t("bundle LocaleNames", () -> {
            ResourceBundle b = ResourceBundle.getBundle(
                    "sun.util.resources.LocaleNames", Locale.ENGLISH);
            p("localenames class", b.getClass().getName());
        });

        // And the display-name surface next door, which reads the same CLDR
        // tree: if these work and currency does not, the gap is the bundle,
        // not the provider machinery.
        p("locale displayCountry", Locale.US.getDisplayCountry(Locale.ENGLISH));
        p("locale displayLanguage", Locale.US.getDisplayLanguage(Locale.ENGLISH));
        p("tz displayName", TimeZone.getTimeZone("America/New_York")
                .getDisplayName(false, TimeZone.LONG, Locale.ENGLISH));

        System.out.println("DONE CurrencyNameProbe");
    }
}
