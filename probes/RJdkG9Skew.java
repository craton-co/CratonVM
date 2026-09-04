import java.util.Locale;

/**
 * G9-1: the Unicode version skew above the BMP, the final sigma the
 * character-wise case arm used to drop, and the three record-component rules.
 *
 * Every expectation is HotSpot 25.0.3+9, captured 2026-08-16. Values are
 * printed as UTF-16 code units, never as rendered characters, and every label
 * is ASCII: a non-ASCII label goes through the Windows console code page
 * differently on the two VMs and fails the differential with every assertion
 * passing.
 */
public class RJdkG9Skew {
    static int checks = 0, fails = 0;

    static void ck(String label, String actual, String want) {
        checks++;
        if (!actual.equals(want)) {
            fails++;
            System.out.println("FAIL " + label + " got=" + actual + " want=" + want);
        }
    }

    static String u16(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) { if (i > 0) b.append(','); b.append((int) s.charAt(i)); }
        return b.toString();
    }

    /** The four measured runs where Rust has a case mapping and JDK 25 has none. */
    static final int[][] SKEW = { {0xA7CE, 0xA7CF}, {0xA7D2, 0xA7D5},
                                  {0x16EA0, 0x16EB8}, {0x16EBB, 0x16ED3} };

    static void skew() {
        int n = 0;
        for (int[] run : SKEW) {
            for (int cp = run[0]; cp <= run[1]; cp++, n++) {
                String s = new String(Character.toChars(cp));
                // Identity in EVERY locale, both directions. Was Rust's pairing
                // for the 50 supplementary members before G9-1.
                for (String tag : new String[] {"und", "tr", "az", "lt", "en", "el"}) {
                    Locale loc = Locale.forLanguageTag(tag);
                    ck("skew.up." + Integer.toHexString(cp) + "." + tag,
                            u16(s.toUpperCase(loc)), u16(s));
                    ck("skew.lo." + Integer.toHexString(cp) + "." + tag,
                            u16(s.toLowerCase(loc)), u16(s));
                }
                // The discriminator a table that PAIRS them answers true to.
                // Rust's partner: XOR 1 inside the A7Cx pairs, +/- 0x1B across
                // the two supplementary runs. A table that pairs them says true.
                int partner = cp < 0x10000 ? (cp ^ 1)
                        : (cp <= 0x16EB8 ? cp + 0x1B : cp - 0x1B);
                ck("skew.eqic." + Integer.toHexString(cp),
                        String.valueOf(s.equalsIgnoreCase(
                                new String(Character.toChars(partner)))), "false");
            }
        }
        ck("skew.count", String.valueOf(n), "56");
        // Controls: real JDK case pairs inside the same spans, and the two
        // code points that split the supplementary block. A range test fails here.
        ck("skew.ctl.a7d1", u16("\uA7D1".toUpperCase(Locale.ROOT)), u16("\uA7D0"));
        ck("skew.ctl.a7d7", u16("\uA7D7".toUpperCase(Locale.ROOT)), u16("\uA7D6"));
        ck("skew.ctl.sharps", u16("\u00DF".toUpperCase(Locale.ROOT)), "83,83");
    }

    static void sigma() {
        // The character-wise arm runs when a skewed code point is present, and
        // char-at-a-time lowercasing has no context. 962 = U+03C2 FINAL SIGMA,
        // 963 = U+03C3 medial. Every row measured.
        ck("sig.unassigned.a7ce", u16("A\u03A3\uA7CE".toLowerCase(Locale.ROOT)), "97,962,42958");
        ck("sig.unassigned.16ea0", u16(("A\u03A3" + new String(Character.toChars(0x16EA0)))
                .toLowerCase(Locale.ROOT)), "97,962,55323,56992");
        // A7D3 IS an assigned lowercase letter on JDK 25, so the sigma stays MEDIAL.
        // This is the control that says the rule is Final_Cased, not "always final".
        ck("sig.assigned.a7d3", u16("A\u03A3\uA7D3".toLowerCase(Locale.ROOT)), "97,963,42963");
        ck("sig.cased.after", u16("\uA7CEA\u03A3A".toLowerCase(Locale.ROOT)), "42958,97,963,97");
        // No skewed code point: the bulk path, which was already right.
        ck("sig.bulk.final", u16("A\u03A3".toLowerCase(Locale.ROOT)), "97,962");
        ck("sig.bulk.pair", u16("\u03A3\u03A3".toLowerCase(Locale.ROOT)), "963,962");
    }

    record Z(boolean b) {}
    record F(float v) {}
    record D(double v) {}
    record Mixed(boolean flag, int n) {}

    static void recs() {
        // Boolean.hashCode, not the int value. Was 0 and 1.
        ck("rec.bool.false", String.valueOf(new Z(false).hashCode()), "1237");
        ck("rec.bool.true", String.valueOf(new Z(true).hashCode()), "1231");
        // 0 and 1 are the ONLY int values at which the two wrappers differ, so
        // an int component holding them must NOT move.
        ck("rec.mixed.f0", String.valueOf(new Mixed(false, 0).hashCode()),
                String.valueOf(1237 * 31));
        ck("rec.mixed.t1", String.valueOf(new Mixed(true, 1).hashCode()),
                String.valueOf(1231 * 31 + 1));
        // floatToIntBits, not floatToRawIntBits: every NaN collapses.
        float oddNan = Float.intBitsToFloat(0x7F800001);
        ck("rec.float.nan.payload", String.valueOf(new F(oddNan).hashCode()), "2143289344");
        ck("rec.float.nan.canon", String.valueOf(new F(Float.NaN).hashCode()), "2143289344");
        double oddDNan = Double.longBitsToDouble(0x7FF0000000000001L);
        ck("rec.double.nan.payload", String.valueOf(new D(oddDNan).hashCode()), "2146959360");
        // Two NaNs with DIFFERENT payloads are equal components. Was false.
        ck("rec.float.nan.equals", String.valueOf(new F(oddNan).equals(new F(Float.NaN))), "true");
        ck("rec.double.nan.equals",
                String.valueOf(new D(oddDNan).equals(new D(Double.NaN))), "true");
        // Controls: the non-NaN rules must not have moved with them.
        ck("rec.float.zeros", String.valueOf(new F(0.0f).equals(new F(-0.0f))), "false");
        ck("rec.double.zeros", String.valueOf(new D(0.0).equals(new D(-0.0))), "false");
        ck("rec.float.negzero.hash", String.valueOf(new F(-0.0f).hashCode()), "-2147483648");
        ck("rec.float.plain", String.valueOf(new F(1.5f).hashCode()), "1069547520");
    }

    static void charfam() {
        // Spot rows from the 1,114,112-code-point sweep, chosen where Rust's
        // tables and the JDK's disagree, so a body that derives from Rust fails.
        ck("chr.isletter.2160", String.valueOf(Character.isLetter(0x2160)), "false");
        ck("chr.isalpha.2160", String.valueOf(Character.isAlphabetic(0x2160)), "true");
        ck("chr.isdigit.0660", String.valueOf(Character.isDigit(0x0660)), "true");
        ck("chr.isdigit.1D7CE", String.valueOf(Character.isDigit(0x1D7CE)), "true");
        ck("chr.isws.001C", String.valueOf(Character.isWhitespace(0x001C)), "true");
        ck("chr.isws.00A0", String.valueOf(Character.isWhitespace(0x00A0)), "false");
        ck("chr.isupper.16EA0", String.valueOf(Character.isUpperCase(0x16EA0)), "false");
        ck("chr.islower.16EBB", String.valueOf(Character.isLowerCase(0x16EBB)), "false");
        ck("chr.islower.0295", String.valueOf(Character.isLowerCase(0x0295)), "true");
        ck("chr.islower.A7F1", String.valueOf(Character.isLowerCase(0xA7F1)), "false");
        ck("chr.toupper.1FB3", String.valueOf(Character.toUpperCase(0x1FB3)), "8124");
        ck("chr.charcount.neg", String.valueOf(Character.charCount(-1)), "1");
        ck("chr.numval.2160", String.valueOf(Character.getNumericValue('\u2160')), "1");
        ck("chr.numval.00BD", String.valueOf(Character.getNumericValue('\u00BD')), "-2");
        // The unregistered (I)I overload runs real CharacterData and must stay right.
        ck("chr.numval.10107.int", String.valueOf(Character.getNumericValue(0x10107)), "1");
        ck("chr.fordigit.oob", String.valueOf((int) Character.forDigit(5, 40)), "0");
        ck("chr.digit.oob", String.valueOf(Character.digit('5', 40)), "-1");
    }

    public static void main(String[] a) {
        skew(); sigma(); recs(); charfam();
        System.out.println("@@RESULT checks=" + checks + " fails=" + fails);
    }
}
