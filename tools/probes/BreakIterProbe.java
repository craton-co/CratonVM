import java.text.BreakIterator;
import java.util.Locale;

/**
 * ConditionalSpecialCasing.isFinalCased walks backwards only while
 * !wordBoundary.isBoundary(i). A spurious boundary between the preceding
 * cased letter and the sigma makes the loop body never run, and the sigma
 * stays medial.
 */
public class BreakIterProbe {
    static void t(String s) {
        BreakIterator b = BreakIterator.getWordInstance(Locale.ROOT);
        b.setText(s);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i <= s.length(); i++) {
            sb.append(i).append(b.isBoundary(i) ? "=B " : "=. ");
        }
        StringBuilder cps = new StringBuilder();
        for (int cp : s.codePoints().toArray()) cps.append(String.format("U+%04X.", cp));
        System.out.println(cps + " len=" + s.length() + "  " + sb);
    }

    public static void main(String[] a) {
        t("AΣ");
        t("ΑΣ");
        t("ABΣ");
        t("ÀΣ");
        t("ΣΣ");
        t("A'Σ");
        t("hello");
        t("a b");
        System.out.println("RESULT done");
    }
}
