import java.text.CharacterIterator;
import java.text.StringCharacterIterator;

/**
 * ConditionalSpecialCasing.isFinalCased walks backwards from the sigma with a
 * StringCharacterIterator positioned at `index`. If previous() misreports, the
 * walk never finds the preceding cased letter and the sigma stays medial.
 */
public class SciProbe {
    static void walk(String s, int index) {
        StringCharacterIterator it = new StringCharacterIterator(s, 0, s.length(), index);
        StringBuilder sb = new StringBuilder();
        sb.append("begin=").append(it.getBeginIndex())
          .append(" end=").append(it.getEndIndex())
          .append(" idx=").append(it.getIndex())
          .append(" current=").append(fmt(it.current()));
        char ch = it.previous();
        sb.append(" previous1=").append(fmt(ch));
        ch = it.previous();
        sb.append(" previous2=").append(fmt(ch));
        System.out.println("walk(" + esc(s) + ", " + index + ") " + sb);
    }

    static String fmt(char c) {
        return c == CharacterIterator.DONE ? "DONE" : ("U+" + String.format("%04X", (int) c));
    }

    static String esc(String s) {
        StringBuilder b = new StringBuilder();
        for (int cp : s.codePoints().toArray()) b.append(String.format("U+%04X.", cp));
        return b.toString();
    }

    public static void main(String[] a) {
        walk("AΣ", 1);
        walk("ΑΣ", 1);
        walk("ABΣ", 2);
        walk("A'Σ", 2);
        // The same reads through String, as a control on the iterator only.
        for (String s : new String[] { "AΣ", "ΑΣ" }) {
            System.out.println("charAt(0)=" + fmt(s.charAt(0))
                    + " codePointBefore(1)=U+" + String.format("%04X", s.codePointBefore(1))
                    + " length=" + s.length() + "  for " + esc(s));
        }
        System.out.println("RESULT done");
    }
}
