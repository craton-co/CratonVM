import java.text.DecimalFormat;
import java.text.NumberFormat;
import java.util.Locale;
import java.util.TreeMap;

/**
 * W7-44 measurement probe. CratonVM answers
 * `sun.util.locale.provider.LocaleResources.getNumberPatterns()` from ONE
 * hardcoded, locale-independent 4-slot array. This probe prints what HotSpot's
 * CLDR data actually holds for every installed locale so the population of
 * wrong answers can be counted instead of guessed.
 *
 * Slot order is the JDK's: 0 = number, 1 = currency, 2 = percent, 3 = scientific.
 *
 * Run:  java probes/NumberPatternCensusProbe.java
 * On CratonVM the same class file must print the same table; every row that
 * differs is one locale the hardcoded array gets wrong.
 */
public final class NumberPatternCensusProbe {

    // The four patterns CratonVM's `getNumberPatterns()` override returns
    // for EVERY locale. Slot 1 read "¤#,##0.00;(¤#,##0.00)" until W7-44 — an
    // accounting negative subpattern that matched 0 of the 1158 installed
    // locales and rendered every negative currency amount in parentheses.
    private static final String[] CRATON = {
        "#,##0.###",
        "¤#,##0.00",
        "#,##0%",
        "#E0",
    };

    public static void main(String[] args) {
        Locale[] all = Locale.getAvailableLocales();
        int total = 0;
        int[] wrong = new int[4];
        int anyWrong = 0;
        // Distinct currency patterns actually observed, with a count.
        TreeMap<String, Integer> currencyForms = new TreeMap<>();
        TreeMap<String, Integer> numberForms = new TreeMap<>();
        TreeMap<String, Integer> percentForms = new TreeMap<>();

        for (Locale loc : all) {
            String[] p = patternsOf(loc);
            if (p == null) {
                continue;
            }
            total++;
            currencyForms.merge(p[1], 1, Integer::sum);
            numberForms.merge(p[0], 1, Integer::sum);
            percentForms.merge(p[2], 1, Integer::sum);
            boolean bad = false;
            for (int i = 0; i < 4; i++) {
                if (!CRATON[i].equals(p[i])) {
                    wrong[i]++;
                    bad = true;
                }
            }
            if (bad) {
                anyWrong++;
            }
        }

        System.out.println("locales.total=" + total);
        System.out.println("wrong.number=" + wrong[0]);
        System.out.println("wrong.currency=" + wrong[1]);
        System.out.println("wrong.percent=" + wrong[2]);
        System.out.println("wrong.scientific=" + wrong[3]);
        System.out.println("wrong.anySlot=" + anyWrong);
        System.out.println("distinct.numberForms=" + numberForms.size());
        System.out.println("distinct.currencyForms=" + currencyForms.size());
        System.out.println("distinct.percentForms=" + percentForms.size());

        System.out.println("--- currency patterns by frequency ---");
        currencyForms.entrySet().stream()
                .sorted((a, b) -> b.getValue() - a.getValue())
                .forEach(e -> System.out.println("  " + e.getValue() + "  " + esc(e.getKey())));
        System.out.println("--- number patterns by frequency ---");
        numberForms.entrySet().stream()
                .sorted((a, b) -> b.getValue() - a.getValue())
                .forEach(e -> System.out.println("  " + e.getValue() + "  " + esc(e.getKey())));
        System.out.println("--- percent patterns by frequency ---");
        percentForms.entrySet().stream()
                .sorted((a, b) -> b.getValue() - a.getValue())
                .forEach(e -> System.out.println("  " + e.getValue() + "  " + esc(e.getKey())));

        System.out.println("--- named locales: pattern + rendered values ---");
        for (String tag : new String[] {
            "en-US", "en-GB", "en-CA", "de-DE", "fr-FR", "ja-JP", "zh-CN",
            "ru-RU", "pt-BR", "es-ES", "it-IT", "nl-NL", "sv-SE", "hi-IN",
            "ar-EG", "tr-TR", "ko-KR", "pl-PL", "he-IL", "th-TH", "und",
        }) {
            Locale loc = Locale.forLanguageTag(tag);
            String[] p = patternsOf(loc);
            System.out.println(tag + ".patterns=" + (p == null ? "n/a"
                    : esc(p[0]) + " | " + esc(p[1]) + " | " + esc(p[2]) + " | " + esc(p[3])));
            System.out.println(tag + ".currencyNeg=" + esc(
                    NumberFormat.getCurrencyInstance(loc).format(-1234.5)));
            System.out.println(tag + ".currencyPos=" + esc(
                    NumberFormat.getCurrencyInstance(loc).format(1234.5)));
            System.out.println(tag + ".currencyZero=" + esc(
                    NumberFormat.getCurrencyInstance(loc).format(0)));
            System.out.println(tag + ".percent=" + esc(
                    NumberFormat.getPercentInstance(loc).format(0.755)));
            System.out.println(tag + ".integer=" + esc(
                    NumberFormat.getIntegerInstance(loc).format(1234567.6)));
            System.out.println(tag + ".number=" + esc(
                    NumberFormat.getNumberInstance(loc).format(1.23456789)));
        }
    }

    /**
     * The JDK does not expose `LocaleResources.getNumberPatterns()` publicly,
     * but each of the four factories builds a `DecimalFormat` straight from the
     * corresponding slot, so `toPattern()` recovers it (modulo the digit-count
     * normalisation `DecimalFormat` applies, which is identical on both sides).
     */
    private static String[] patternsOf(Locale loc) {
        try {
            return new String[] {
                ((DecimalFormat) NumberFormat.getNumberInstance(loc)).toPattern(),
                ((DecimalFormat) NumberFormat.getCurrencyInstance(loc)).toPattern(),
                ((DecimalFormat) NumberFormat.getPercentInstance(loc)).toPattern(),
                // NumberFormat.getScientificInstance(Locale) is package-private;
                // slot 3 is not reachable from outside java.text, so it is
                // reported as the value CratonVM hardcodes and never counted wrong.
                CRATON[3],
            };
        } catch (RuntimeException e) {
            return null;
        }
    }

    /** Escape non-ASCII so the transcript is diffable regardless of console encoding. */
    private static String esc(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7E) {
                sb.append(String.format("\\u%04x", (int) c));
            } else {
                sb.append(c);
            }
        }
        return sb.toString();
    }
}
