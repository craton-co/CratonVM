import java.lang.reflect.Constructor;
import java.lang.reflect.Method;
import java.text.DateFormatSymbols;
import java.text.DecimalFormatSymbols;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.Locale;
import java.util.Map;

/**
 * W7-80: verify the CLDR merge algorithm, not the locale report.
 *
 * This is NOT a competitor to {@code DefaultLocaleProbe}. That probe measures
 * what a VM answers; this one measures whether the *chain* CratonVM's
 * {@code load_cldr_table} walks — ROOT, then {@code _<lang>}, then
 * {@code _<lang>_<country>}, merged least-specific-first out of
 * {@code sun.text.resources.cldr[.ext]} — reconstructs the same table the real
 * {@code DateFormatSymbols} / {@code DecimalFormatSymbols} get. It reimplements
 * the Rust algorithm in Java and diffs it against the JDK's own answer for
 * every installed locale.
 *
 * The point is to find the locales where the flattened chain is NOT enough,
 * before shipping. CLDR's real parent chain is not always truncation
 * (`en_GB` -> `en_001` -> `en`, `es_MX` -> `es_419` -> `es`) and the script
 * subtag is not modelled, so a non-zero miss count is expected; what matters is
 * WHICH locales miss and by how much.
 *
 * Run with:
 *   java --add-opens jdk.localedata/sun.text.resources.cldr.ext=ALL-UNNAMED \
 *        --add-opens java.base/sun.text.resources.cldr=ALL-UNNAMED \
 *        CldrChainProbe.java
 *
 * The --add-opens are needed HERE and only here: this probe reaches
 * getContents() reflectively from an unnamed module. CratonVM's native reaches
 * it from Rust and is not subject to that check — measured, see W7-80.
 */
public final class CldrChainProbe {

    static Map<String, Object> merge(String simple, Locale l) {
        Map<String, Object> out = new LinkedHashMap<>();
        String lang = l.getLanguage();
        String country = l.getCountry();
        for (String suffix : suffixes(lang, country)) {
            for (String pkg : new String[] {
                "sun.text.resources.cldr", "sun.text.resources.cldr.ext",
            }) {
                Class<?> c;
                try {
                    c = Class.forName(pkg + "." + simple + suffix);
                } catch (Throwable t) {
                    continue;
                }
                try {
                    Constructor<?> ctor = c.getDeclaredConstructor();
                    ctor.setAccessible(true);
                    Method m = c.getDeclaredMethod("getContents");
                    m.setAccessible(true);
                    for (Object[] row : (Object[][]) m.invoke(ctor.newInstance())) {
                        out.put((String) row[0], row[1]);
                    }
                } catch (Throwable t) {
                    // fall through: a candidate that will not evaluate leaves
                    // whatever the less-specific ones already merged
                }
                break;
            }
        }
        return out;
    }

    static String[] suffixes(String lang, String country) {
        if (lang.isEmpty()) {
            return new String[] {""};
        }
        if (country.isEmpty()) {
            return new String[] {"", "_" + lang};
        }
        return new String[] {"", "_" + lang, "_" + lang + "_" + country};
    }

    static String show(Object v) {
        return v instanceof Object[] ? Arrays.toString((Object[]) v) : String.valueOf(v);
    }

    /** The `getNumberStrings` rule: <defaultNumberingSystem>.KEY, then latn.KEY, then KEY. */
    static Object numberStrings(Map<String, Object> t, String kind) {
        Object ns = t.get("DefaultNumberingSystem");
        if (ns instanceof String && t.containsKey(ns + "." + kind)) {
            return t.get(ns + "." + kind);
        }
        if (t.containsKey("latn." + kind)) {
            return t.get("latn." + kind);
        }
        return t.get(kind);
    }

    public static void main(String[] args) {
        int total = 0;
        int missMonths = 0;
        int missShortMonths = 0;
        int missWeekdays = 0;
        int missEras = 0;
        int missDecimal = 0;
        int missGrouping = 0;
        int noChain = 0;
        StringBuilder examples = new StringBuilder();

        for (Locale l : Locale.getAvailableLocales()) {
            if (l.getLanguage().isEmpty()) {
                continue;
            }
            total++;
            Map<String, Object> t = merge("FormatData", l);
            if (t.isEmpty()) {
                noChain++;
                continue;
            }
            DateFormatSymbols real = DateFormatSymbols.getInstance(l);
            DecimalFormatSymbols dec = DecimalFormatSymbols.getInstance(l);

            boolean m = !Arrays.equals((Object[]) t.get("MonthNames"), real.getMonths());
            boolean sm = !Arrays.equals((Object[]) t.get("MonthAbbreviations"), real.getShortMonths());
            // DayNames is 0-based in the bundle and 1-based on DateFormatSymbols.
            Object[] days = (Object[]) t.get("DayNames");
            String[] realDays = real.getWeekdays();
            boolean wd = days == null || realDays.length != days.length + 1;
            if (!wd) {
                for (int i = 0; i < days.length; i++) {
                    if (!String.valueOf(days[i]).equals(realDays[i + 1])) {
                        wd = true;
                        break;
                    }
                }
            }
            Object eras = t.containsKey("Eras") ? t.get("Eras") : t.get("long.Eras");
            boolean er = !Arrays.equals((Object[]) eras, real.getEras());

            Object[] ne = (Object[]) numberStrings(t, "NumberElements");
            boolean dsep = ne == null
                || String.valueOf(ne[0]).charAt(0) != dec.getDecimalSeparator();
            boolean gsep = ne == null
                || String.valueOf(ne[1]).charAt(0) != dec.getGroupingSeparator();

            if (m) { missMonths++; }
            if (sm) { missShortMonths++; }
            if (wd) { missWeekdays++; }
            if (er) { missEras++; }
            if (dsep) { missDecimal++; }
            if (gsep) { missGrouping++; }

            if ((m || sm || dsep || gsep) && examples.length() < 3000) {
                examples.append("  MISS ").append(l).append(" months=").append(m)
                        .append(" shortMonths=").append(sm)
                        .append(" dec=").append(dsep).append(" grp=").append(gsep)
                        .append("\n    chain.shortMonths=").append(show(t.get("MonthAbbreviations")))
                        .append("\n    real.shortMonths =").append(Arrays.toString(real.getShortMonths()))
                        .append('\n');
            }
        }

        System.out.println("locales.total=" + total);
        System.out.println("chain.empty=" + noChain);
        System.out.println("miss.months=" + missMonths);
        System.out.println("miss.shortMonths=" + missShortMonths);
        System.out.println("miss.weekdays=" + missWeekdays);
        System.out.println("miss.eras=" + missEras);
        System.out.println("miss.decimalSeparator=" + missDecimal);
        System.out.println("miss.groupingSeparator=" + missGrouping);
        System.out.println("---- examples ----");
        System.out.print(examples);

        // The four locales W7-80 measured by hand, printed in full so the
        // record's tables can be checked against a rerun.
        for (Locale l : new Locale[] {
            Locale.US, new Locale("ru", "RU"), Locale.GERMANY,
            new Locale("tr", "TR"), Locale.JAPAN, Locale.UK,
        }) {
            Map<String, Object> t = merge("FormatData", l);
            System.out.println("== " + l);
            System.out.println("  MonthAbbreviations=" + show(t.get("MonthAbbreviations")));
            System.out.println("  NumberPatterns=" + show(numberStrings(t, "NumberPatterns")));
            System.out.println("  NumberElements=" + show(numberStrings(t, "NumberElements")));
            System.out.println("  DatePatterns=" + show(t.get("DatePatterns")));
            System.out.println("  TimePatterns=" + show(t.get("TimePatterns")));
        }
    }
}
