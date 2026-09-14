package cratonvm;

import java.util.Locale;

/**
 * Differential fixture for the locale-sensitive half of
 * {@code String.toUpperCase(Locale)} / {@code toLowerCase(Locale)} — the
 * Turkish/Azeri dotted-I and Lithuanian retained-dot rules of
 * {@code SpecialCasing.txt}, plus the unconditional mappings that must survive
 * unchanged on the locale-dependent path.
 *
 * Every character is printed as an escape so a mismatch is readable in the
 * report rather than mangled by the console encoding.
 */
public class DiffLocaleCase {

    static String esc(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7f) {
                b.append(c);
            } else {
                b.append('\\').append('u');
                String hex = Integer.toHexString(c);
                for (int p = hex.length(); p < 4; p++) {
                    b.append('0');
                }
                b.append(hex);
            }
        }
        return b.toString();
    }

    private static final String[] INPUTS = {
        "title", "TITLE", "istanbul", "ISTANBUL", "I", "i",
        "İ", "ı", "İ", "i̇", "Í",
        "straße", "ΣΣ", "Ì", "Í", "Ĩ",
        "J", "Į", "abc-123", "",
    };

    private static final String[] LANGS = { "", "en", "de", "tr", "az", "lt" };

    public static void main(String[] args) {
        for (String in : INPUTS) {
            System.out.println("in=" + esc(in)
                + " U=" + esc(in.toUpperCase())
                + " L=" + esc(in.toLowerCase()));
            for (String lang : LANGS) {
                Locale loc = lang.isEmpty() ? Locale.ROOT : new Locale(lang);
                System.out.println("  " + (lang.isEmpty() ? "root" : lang)
                    + " U=" + esc(in.toUpperCase(loc))
                    + " L=" + esc(in.toLowerCase(loc)));
            }
        }
        // `Locale.forLanguageTag` reaches the same rules through a different
        // Locale construction path than `new Locale(...)`.
        System.out.println("tag-tr U=" + esc("title".toUpperCase(Locale.forLanguageTag("tr")))
            + " L=" + esc("TITLE".toLowerCase(Locale.forLanguageTag("tr"))));
        System.out.println("tag-lt L=" + esc("Ì".toLowerCase(Locale.forLanguageTag("lt"))));
        // A locale-dependent result must not be served from a receiver-keyed
        // cache populated by an earlier root-locale call on the same String.
        String shared = "TITLE";
        System.out.println("cache root=" + esc(shared.toLowerCase(Locale.ROOT))
            + " tr=" + esc(shared.toLowerCase(new Locale("tr")))
            + " root2=" + esc(shared.toLowerCase(Locale.ROOT)));
        warm();
    }

    /**
     * Same checks again, after enough iterations to tier the callers up.
     *
     * `String.toLowerCase(Locale)` has a thin direct-call helper the JIT emits
     * in place of the native-dispatch round trip; that helper used to carry its
     * own copy of the mapping and ignore the Locale, so the compiled answer
     * disagreed with the interpreted one for exactly the locales this fixture
     * exists to check. 5000 iterations is comfortably past tier-up — the
     * divergence reproduces from about 3000.
     */
    static void warm() {
        Locale tr = new Locale("tr");
        Locale lt = new Locale("lt");
        String[] ins = { "TITLE", "title", "ISTANBUL", "I", "i", "Ì" };
        String last = "";
        for (int i = 0; i < 5000; i++) {
            StringBuilder b = new StringBuilder();
            for (String in : ins) {
                b.append(lower(in, tr)).append('|').append(upper(in, tr)).append('|');
                b.append(lower(in, lt)).append('|').append(upper(in, lt)).append('|');
                b.append(lower(in, Locale.ROOT)).append('|').append(upper(in, Locale.ROOT)).append('|');
                b.append(in.toLowerCase()).append('|').append(in.toUpperCase()).append('#');
            }
            last = b.toString();
        }
        System.out.println("jit " + esc(last));
    }

    static String lower(String s, Locale l) { return s.toLowerCase(l); }
    static String upper(String s, Locale l) { return s.toUpperCase(l); }
}
