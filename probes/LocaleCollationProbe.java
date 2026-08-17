import java.text.Collator;
import java.util.*;

/**
 * The two halves of the H2 `SET COLLATION TURKISH` failure, measured together:
 * the display-name table that H2's CompareMode.getName round-trips through, and
 * the per-locale collation rules that would then have to be right for the
 * collation to mean anything.
 *
 * Run under HotSpot and CratonVM and diff.
 */
public class LocaleCollationProbe {
    static void line(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    /** H2's org.h2.value.CompareMode.getName, verbatim in behaviour. */
    static String h2Name(Locale l) {
        Locale english = Locale.ENGLISH;
        String name = l.getDisplayLanguage(english) + ' '
                    + l.getDisplayCountry(english) + ' ' + l.getVariant();
        return name.trim().replace(' ', '_').toUpperCase(Locale.ENGLISH);
    }

    public static void main(String[] a) {
        // --- half 1: display names ---
        for (String tag : new String[]{"tr", "de", "en", "fr", "es", "it", "ru", "ja", "zh", "pt", "nl", "sv", "pl", "cs", "da"}) {
            Locale l = new Locale(tag);
            line("displayLanguage(" + tag + ",ENGLISH)", l.getDisplayLanguage(Locale.ENGLISH));
        }
        for (String[] lc : new String[][]{{"en","US"},{"en","GB"},{"de","DE"},{"pt","BR"},{"zh","CN"}}) {
            Locale l = new Locale(lc[0], lc[1]);
            line("displayCountry(" + lc[0] + "_" + lc[1] + ",ENGLISH)", l.getDisplayCountry(Locale.ENGLISH));
            line("h2Name(" + lc[0] + "_" + lc[1] + ")", h2Name(l));
        }
        line("h2Name(tr)", h2Name(new Locale("tr")));
        line("h2Name(de)", h2Name(new Locale("de")));

        // How many collation locales have a real English display name?
        Locale[] cl = Collator.getAvailableLocales();
        int named = 0;
        for (Locale l : cl) {
            String dn = l.getDisplayLanguage(Locale.ENGLISH);
            if (dn != null && !dn.isEmpty() && !dn.equals(l.getLanguage())) named++;
        }
        line("collationLocales", cl.length);
        line("collationLocalesWithEnglishName", named);

        // The exact lookup H2 performs for SET COLLATION TURKISH.
        Locale resolved = null;
        for (Locale l : cl) {
            if (h2Name(l).equals("TURKISH")) { resolved = l; break; }
        }
        line("H2 resolves TURKISH to", resolved);

        // --- half 2: do the collators actually carry per-locale rules? ---
        String[][] cases = {
            {"tr", "I", "i", "Turkish: dotted/dotless I are distinct letters"},
            {"tr", "İ", "I", "Turkish: dotted capital I vs ASCII I"},
            {"tr", "ı", "i", "Turkish: dotless i vs ASCII i"},
            {"en", "I", "i", "English control"},
            {"da", "æ", "z", "Danish: ae sorts after z"},
            {"sv", "ä", "z", "Swedish: a-diaeresis sorts after z"},
            {"de", "ä", "z", "German control: a-diaeresis sorts with a"},
        };
        for (String[] c : cases) {
            Collator col = Collator.getInstance(new Locale(c[0]));
            col.setStrength(Collator.IDENTICAL);
            line("compare[" + c[0] + "](" + c[1] + "," + c[2] + ")", Integer.signum(col.compare(c[1], c[2])));
        }
        // Turkish equality at PRIMARY strength — what H2's collation actually uses
        Collator tr = Collator.getInstance(new Locale("tr"));
        tr.setStrength(Collator.PRIMARY);
        line("tr PRIMARY equals(I,i)", tr.compare("I", "i") == 0);
        line("tr PRIMARY equals(İ,I)", tr.compare("İ", "I") == 0);
        line("tr PRIMARY equals(ı,i)", tr.compare("ı", "i") == 0);
        Collator en = Collator.getInstance(Locale.ENGLISH);
        en.setStrength(Collator.PRIMARY);
        line("en PRIMARY equals(I,i)", en.compare("I", "i") == 0);
        System.out.println("PROBE_END");
    }
}
