/** java.lang.Character, swept EXHAUSTIVELY over every code point.
 *
 *  Character's methods are pure functions of a code point, so this needs no
 *  sampling and no oracle file: run it on two VMs and diff stdout. Per-block
 *  checksums keep the output small while still localising any disagreement to
 *  a 4096-code-point block, and the final total catches a difference that
 *  happens to cancel within a block. */
public class CharacterSweep {
    public static void main(String[] a) {
        final int MAX = 0x110000;
        long grand = 0;
        for (int base = 0; base < MAX; base += 4096) {
            long h = 1469598103934665603L;
            for (int cp = base; cp < base + 4096 && cp < MAX; cp++) {
                long v = 0;
                v = v * 31 + (Character.isLetter(cp) ? 1 : 0);
                v = v * 31 + (Character.isDigit(cp) ? 1 : 0);
                v = v * 31 + (Character.isLetterOrDigit(cp) ? 1 : 0);
                v = v * 31 + (Character.isAlphabetic(cp) ? 1 : 0);
                v = v * 31 + (Character.isIdeographic(cp) ? 1 : 0);
                v = v * 31 + (Character.isLowerCase(cp) ? 1 : 0);
                v = v * 31 + (Character.isUpperCase(cp) ? 1 : 0);
                v = v * 31 + (Character.isTitleCase(cp) ? 1 : 0);
                v = v * 31 + (Character.isWhitespace(cp) ? 1 : 0);
                v = v * 31 + (Character.isSpaceChar(cp) ? 1 : 0);
                v = v * 31 + (Character.isISOControl(cp) ? 1 : 0);
                v = v * 31 + (Character.isDefined(cp) ? 1 : 0);
                v = v * 31 + (Character.isMirrored(cp) ? 1 : 0);
                v = v * 31 + (Character.isJavaIdentifierStart(cp) ? 1 : 0);
                v = v * 31 + (Character.isJavaIdentifierPart(cp) ? 1 : 0);
                v = v * 31 + (Character.isUnicodeIdentifierStart(cp) ? 1 : 0);
                v = v * 31 + (Character.isUnicodeIdentifierPart(cp) ? 1 : 0);
                v = v * 31 + (Character.isIdentifierIgnorable(cp) ? 1 : 0);
                v = v * 31 + Character.getType(cp);
                v = v * 31 + Character.getDirectionality(cp);
                v = v * 31 + Character.toLowerCase(cp);
                v = v * 31 + Character.toUpperCase(cp);
                v = v * 31 + Character.toTitleCase(cp);
                v = v * 31 + Character.digit(cp, 36);
                v = v * 31 + Character.getNumericValue(cp);
                v = v * 31 + Character.charCount(cp);
                if (cp <= 0xFFFF) {
                    char c = (char) cp;
                    v = v * 31 + (Character.isHighSurrogate(c) ? 1 : 0);
                    v = v * 31 + (Character.isLowSurrogate(c) ? 1 : 0);
                    v = v * 31 + Character.reverseBytes(c);
                }
                h = (h ^ v) * 1099511628211L;
            }
            System.out.println(Integer.toHexString(base) + " " + h);
            grand = grand * 31 + h;
        }
        // forDigit / MIN_RADIX..MAX_RADIX, independent of the sweep above
        long r = 0;
        for (int radix = Character.MIN_RADIX; radix <= Character.MAX_RADIX; radix++)
            for (int d = -1; d <= radix; d++)
                r = r * 31 + Character.forDigit(d, radix);
        System.out.println("forDigit " + r);
        System.out.println("GRAND " + grand);
        System.out.println("DONE CharacterSweep");
    }
}
