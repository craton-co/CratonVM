/**
 * G9-1's final-sigma divergence, one layer down.
 *
 * String.toLowerCase(Locale) is not a registered native, so the JDK's own
 * ConditionalSpecialCasing runs and decides the final sigma by asking
 * Character.getType about the neighbouring code points. "AΣ" gives the wrong
 * answer and "ΣΣ" gives the right one, so the suspicion is getType, not the
 * sigma logic.
 */
public class CasedProbe {
    static void t(int cp) {
        System.out.println("getType(U+" + String.format("%04X", cp) + ") = " + Character.getType(cp)
                + "  isUpper=" + Character.isUpperCase(cp)
                + "  isLower=" + Character.isLowerCase(cp)
                + "  isLetter=" + Character.isLetter(cp)
                + "  isTitle=" + Character.isTitleCase(cp));
    }

    public static void main(String[] a) {
        // UPPERCASE_LETTER = 1, LOWERCASE_LETTER = 2, TITLECASE_LETTER = 3
        for (int cp : new int[] { 'A', 'Z', 'a', 'z', '0', ' ', '_',
                                  0x03A3 /* Σ */, 0x03C3 /* σ */, 0x03C2 /* ς */,
                                  0x00C0 /* À */, 0x01C5 /* ǅ titlecase */,
                                  0xA7CE, 0xA7D3, 0x16EA0 }) {
            t(cp);
        }
        // The three strings the vector disagrees on, rendered as code points.
        for (String s : new String[] { "AΣ", "ΣΣ", "AΣ꟎" }) {
            StringBuilder sb = new StringBuilder();
            for (int cp : s.toLowerCase(java.util.Locale.ROOT).codePoints().toArray()) {
                if (sb.length() > 0) sb.append(',');
                sb.append(cp);
            }
            System.out.println("lower(" + escape(s) + ") = " + sb);
        }
        System.out.println("RESULT done");
    }

    static String escape(String s) {
        StringBuilder b = new StringBuilder();
        for (int cp : s.codePoints().toArray()) b.append(String.format("U+%04X ", cp));
        return b.toString().trim();
    }
}
