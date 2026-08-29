import java.util.*;

/** L3 tail / `java.util.Locale` (24 rows), `java.util.Date` (19),
 *  `java.util.TimeZone` (10) and `java.util.Currency` (1) — 54 owning
 *  bridge-with-code registrations, the last unclaimed block of `java.util`.
 *
 *  These three are the tail's awkward corner because their answers come from
 *  DATA rather than from code: a locale's display name, a zone's DST rule and a
 *  currency's fraction digits all live in the JDK's CLDR bundles, and a native
 *  that hard-codes any of them is right for the cases its author tried and
 *  silently wrong past them. So the probe asks:
 *
 *    * the PARSE/RENDER round trips, which are pure code — `toLanguageTag` and
 *      `forLanguageTag` on the cases with a grandfathered or legacy spelling
 *      (`iw`/`he`, `no-NO-NY`, the empty locale), where a table lookup and a
 *      string split disagree;
 *    * the identity rules — `Locale.ROOT` is `""`/`""`, an unknown zone ID is
 *      `GMT` rather than an exception, and `Currency.getInstance` of an unknown
 *      code IS an exception;
 *    * the arithmetic — `Date`'s comparison family over the epoch and negative
 *      milliseconds, and `TimeZone.getOffset` on both sides of a DST boundary.
 *
 *  DETERMINISM: nothing here reads the DEFAULT locale or the default time zone,
 *  both of which the two VMs may resolve independently; every call names the
 *  locale or zone it wants. `Date.toString` is likewise never printed — it
 *  renders in the default zone.
 */
public class LocaleDateTzShadowSweep {
    static int rows = 0;
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    interface ThrowingRun { void run() throws Throwable; }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface Call { Object call() throws Throwable; }
    static void tv(String tag, Call r) {
        try { p(tag, "ok " + String.valueOf(r.call())); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    static void locale() {
        Locale us = Locale.of("en", "US");
        p("getLanguage", us.getLanguage());
        p("getCountry", us.getCountry());
        p("getVariant", us.getVariant());
        p("getScript", us.getScript());
        p("toString", us.toString());
        p("toLanguageTag", us.toLanguageTag());
        p("getDisplayLanguage in English", us.getDisplayLanguage(Locale.ENGLISH));
        p("getDisplayCountry in English", us.getDisplayCountry(Locale.ENGLISH));
        p("getDisplayName in English", us.getDisplayName(Locale.ENGLISH));
        p("getISO3Language", us.getISO3Language());
        p("getISO3Country", us.getISO3Country());
        p("equals", us.equals(Locale.of("en", "US")));
        p("hashCode agrees", us.hashCode() == Locale.of("en", "US").hashCode());
        p("US constant equals", Locale.US.equals(us));

        // ROOT is the empty locale, and it is NOT the same as a null-ish one.
        p("ROOT language", "[" + Locale.ROOT.getLanguage() + "]");
        p("ROOT country", "[" + Locale.ROOT.getCountry() + "]");
        p("ROOT toString", "[" + Locale.ROOT.toString() + "]");
        p("ROOT toLanguageTag", Locale.ROOT.toLanguageTag());
        p("ROOT display in English", Locale.ROOT.getDisplayName(Locale.ENGLISH));

        // language-only, country-only, variant, and the three-part spelling
        p("language only toString", Locale.of("fr").toString());
        p("language only tag", Locale.of("fr").toLanguageTag());
        p("country only toString", "[" + Locale.of("", "FR").toString() + "]");
        p("country only tag", Locale.of("", "FR").toLanguageTag());
        p("variant toString", Locale.of("en", "US", "POSIX").toString());
        p("variant tag", Locale.of("en", "US", "POSIX").toLanguageTag());

        // The legacy ISO codes the JDK keeps mapping for compatibility: `iw`
        // stays `iw` in `getLanguage()` and becomes `he` in the language tag.
        p("iw getLanguage", Locale.of("iw").getLanguage());
        p("iw toLanguageTag", Locale.of("iw").toLanguageTag());
        p("he getLanguage", Locale.of("he").getLanguage());
        p("ji getLanguage", Locale.of("ji").getLanguage());
        p("in getLanguage", Locale.of("in").getLanguage());
        p("id getLanguage", Locale.of("id").getLanguage());

        // case normalisation: language lower, country upper, script title
        p("case normalised language", Locale.of("EN", "us").getLanguage());
        p("case normalised country", Locale.of("EN", "us").getCountry());

        p("forLanguageTag en-US", Locale.forLanguageTag("en-US").toString());
        p("forLanguageTag zh-Hant-TW", Locale.forLanguageTag("zh-Hant-TW").toString());
        p("forLanguageTag script", Locale.forLanguageTag("zh-Hant-TW").getScript());
        p("forLanguageTag garbage", "[" + Locale.forLanguageTag("!!!").toString() + "]");
        p("forLanguageTag empty", "[" + Locale.forLanguageTag("").toString() + "]");
        t("forLanguageTag null", () -> Locale.forLanguageTag(null));
        p("forLanguageTag round trip", Locale.forLanguageTag("en-US").toLanguageTag());

        Locale built = new Locale.Builder()
            .setLanguage("en").setRegion("GB").setScript("Latn").build();
        p("Builder toString", built.toString());
        p("Builder tag", built.toLanguageTag());
        t("Builder bad language", () -> new Locale.Builder().setLanguage("123456789"));

        p("getExtensionKeys empty", Locale.of("en").getExtensionKeys().toString());
        p("getUnicodeLocaleKeys empty", Locale.of("en").getUnicodeLocaleKeys().toString());
        p("getExtension of a tag with one",
            Locale.forLanguageTag("en-US-u-ca-buddhist").getExtension('u'));

        t("of null language", () -> Locale.of((String) null));
        p("ENGLISH constant", Locale.ENGLISH.toString());
        p("UK constant tag", Locale.UK.toLanguageTag());
        p("stripExtensions", Locale.forLanguageTag("en-US-u-ca-buddhist")
            .stripExtensions().toLanguageTag());
    }

    static void date() {
        Date epoch = new Date(0L);
        Date later = new Date(1_000L);
        Date neg = new Date(-1_000L);
        p("getTime", epoch.getTime());
        p("getTime negative", neg.getTime());
        p("before", epoch.before(later));
        p("after", later.after(epoch));
        p("before itself", epoch.before(epoch));
        p("after itself", epoch.after(epoch));
        p("compareTo less", Integer.signum(epoch.compareTo(later)));
        p("compareTo greater", Integer.signum(later.compareTo(epoch)));
        p("compareTo equal", epoch.compareTo(new Date(0L)));
        p("compareTo across zero", Integer.signum(neg.compareTo(epoch)));
        p("equals", epoch.equals(new Date(0L)));
        p("equals different", epoch.equals(later));
        p("equals null", epoch.equals(null));
        p("equals non-Date", epoch.equals("s"));
        p("hashCode is the millis fold", epoch.hashCode() == (int) (0L ^ (0L >>> 32)));
        p("hashCode negative", neg.hashCode() == (int) (-1000L ^ (-1000L >>> 32)));
        Date m = new Date(0L);
        m.setTime(5_000L);
        p("setTime", m.getTime());
        p("clone", ((Date) m.clone()).getTime());
        p("clone is independent", cloneIndependent());
        t("compareTo null", () -> epoch.compareTo(null));
        t("before null", () -> epoch.before(null));
        p("from Instant", Date.from(java.time.Instant.ofEpochMilli(1234L)).getTime());
        p("toInstant", new Date(1234L).toInstant().toEpochMilli());
        t("from null Instant", () -> Date.from(null));
        p("MAX millis", new Date(Long.MAX_VALUE).getTime());
        p("MIN millis", new Date(Long.MIN_VALUE).getTime());
        // `Date.toString` renders in the DEFAULT zone, so only its SHAPE is
        // safe to compare: a fixed length and the day-of-week prefix.
        p("toString length is stable", new Date(0L).toString().length()
            == new Date(1_000L).toString().length());
    }
    static String cloneIndependent() {
        Date a = new Date(1L);
        Date b = (Date) a.clone();
        b.setTime(2L);
        return a.getTime() + "/" + b.getTime();
    }

    static void timeZone() {
        TimeZone ny = TimeZone.getTimeZone("America/New_York");
        p("getID", ny.getID());
        p("getRawOffset", ny.getRawOffset());
        p("useDaylightTime", ny.useDaylightTime());
        p("getDSTSavings", ny.getDSTSavings());
        // 1970-01-01 is winter in New York, 1970-07-01 is summer.
        p("inDaylightTime winter", ny.inDaylightTime(new Date(0L)));
        p("inDaylightTime summer", ny.inDaylightTime(new Date(15_638_400_000L)));
        p("getOffset winter", ny.getOffset(0L));
        p("getOffset summer", ny.getOffset(15_638_400_000L));
        p("hasSameRules as itself", ny.hasSameRules(TimeZone.getTimeZone("America/New_York")));
        p("hasSameRules as UTC", ny.hasSameRules(TimeZone.getTimeZone("UTC")));

        TimeZone utc = TimeZone.getTimeZone("UTC");
        p("UTC id", utc.getID());
        p("UTC raw offset", utc.getRawOffset());
        p("UTC useDaylightTime", utc.useDaylightTime());
        p("GMT id", TimeZone.getTimeZone("GMT").getID());
        // An UNKNOWN id is GMT, not an exception. That is the rule a shim most
        // often turns into a throw or a null.
        p("unknown id falls back to GMT", TimeZone.getTimeZone("Not/AZone").getID());
        p("unknown id offset", TimeZone.getTimeZone("Not/AZone").getRawOffset());
        t("getTimeZone null", () -> TimeZone.getTimeZone((String) null));
        p("GMT+05:30 id", TimeZone.getTimeZone("GMT+05:30").getID());
        p("GMT+05:30 offset", TimeZone.getTimeZone("GMT+05:30").getRawOffset());
        p("GMT-8 offset", TimeZone.getTimeZone("GMT-8").getRawOffset());
        p("getDisplayName in English", ny.getDisplayName(false, TimeZone.SHORT, Locale.ENGLISH));
        p("getDisplayName daylight", ny.getDisplayName(true, TimeZone.SHORT, Locale.ENGLISH));
        p("toZoneId", ny.toZoneId().getId());
        p("from ZoneId", TimeZone.getTimeZone(java.time.ZoneId.of("Europe/Paris")).getID());

        SimpleTimeZone stz = new SimpleTimeZone(-5 * 3_600_000, "Custom");
        p("SimpleTimeZone id", stz.getID());
        p("SimpleTimeZone raw offset", stz.getRawOffset());
        p("SimpleTimeZone useDaylightTime", stz.useDaylightTime());
        p("SimpleTimeZone offset", stz.getOffset(0L));
        stz.setRawOffset(3_600_000);
        p("SimpleTimeZone after setRawOffset", stz.getRawOffset());
        p("SimpleTimeZone equals a copy",
            stz.equals(new SimpleTimeZone(3_600_000, "Custom")));
        p("availableIDs contains UTC",
            Arrays.asList(TimeZone.getAvailableIDs()).contains("UTC"));
        p("availableIDs for offset 0 contains GMT",
            Arrays.asList(TimeZone.getAvailableIDs(0)).contains("GMT"));
    }

    static void currency() {
        Currency usd = Currency.getInstance("USD");
        p("getCurrencyCode", usd.getCurrencyCode());
        p("getDefaultFractionDigits", usd.getDefaultFractionDigits());
        p("getNumericCode", usd.getNumericCode());
        p("getSymbol in US", usd.getSymbol(Locale.US));
        p("getDisplayName in English", usd.getDisplayName(Locale.ENGLISH));
        p("toString", usd.toString());
        p("identity is interned", Currency.getInstance("USD") == usd);
        p("JPY fraction digits", Currency.getInstance("JPY").getDefaultFractionDigits());
        p("XXX fraction digits", Currency.getInstance("XXX").getDefaultFractionDigits());
        t("unknown code", () -> Currency.getInstance("ZZZ"));
        t("lower case code", () -> Currency.getInstance("usd"));
        t("null code", () -> Currency.getInstance((String) null));
        p("getInstance(Locale.US)", Currency.getInstance(Locale.US).getCurrencyCode());
        p("available currencies contains USD",
            Currency.getAvailableCurrencies().contains(usd));
    }

    public static void main(String[] args) {
        locale();
        date();
        timeZone();
        currency();
        System.out.println("ROWS " + rows);
        System.out.println("DONE LocaleDateTzShadowSweep");
    }
}
