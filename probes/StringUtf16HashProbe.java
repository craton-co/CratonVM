/**
 * Why does `String.hashCode()` disagree with HotSpot for a non-Latin-1 string
 * once the forced native is gone?
 *
 * Dropping the `java/lang/String` `Bridge` surface hands `hashCode` back to the
 * real JDK bytecode, which is `isLatin1() ? StringLatin1.hashCode(value)
 * : StringUTF16.hashCode(value)`. The Latin-1 side agrees; the UTF-16 side does
 * not. This probe narrows that down without guessing, by computing the same
 * answer four ways and printing all four:
 *
 *   1. `s.hashCode()` — whatever the VM actually runs;
 *   2. the JLS formula over `charAt` — the contract, computed in plain Java;
 *   3. the same formula over `toCharArray()`;
 *   4. `Arrays.hashCode(char[])`-style over the code units.
 *
 * If (1) disagrees with (2) but (2)/(3)/(4) agree with each other, the defect is
 * inside whatever `hashCode` dispatches to, not in the string's contents. If
 * (2) disagrees too, the string itself is wrong and `hashCode` is a symptom.
 *
 * It also prints `coder`-visible facts (length, isLatin1-by-proxy) so the two
 * cases can be told apart on a run where only one is broken.
 */
public class StringUtf16HashProbe {

    static String u(int... units) {
        char[] c = new char[units.length];
        for (int i = 0; i < units.length; i++) {
            c[i] = (char) units[i];
        }
        return new String(c);
    }

    /** The JLS definition of String.hashCode, computed from charAt. */
    static int jlsHash(String s) {
        int h = 0;
        for (int i = 0; i < s.length(); i++) {
            h = 31 * h + s.charAt(i);
        }
        return h;
    }

    static int jlsHashFromArray(String s) {
        int h = 0;
        for (char c : s.toCharArray()) {
            h = 31 * h + c;
        }
        return h;
    }

    static void report(String tag, String s) {
        StringBuilder units = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) {
                units.append(' ');
            }
            units.append(String.format("%04X", (int) s.charAt(i)));
        }
        // A string is Latin-1-representable iff every code unit is < 0x100.
        boolean latin1able = true;
        for (int i = 0; i < s.length(); i++) {
            if (s.charAt(i) > 0xFF) {
                latin1able = false;
            }
        }
        System.out.println(tag
                + " len=" + s.length()
                + " latin1able=" + latin1able
                + " units=[" + units + "]"
                + " hashCode=" + s.hashCode()
                + " jlsFromCharAt=" + jlsHash(s)
                + " jlsFromArray=" + jlsHashFromArray(s)
                + " agree=" + (s.hashCode() == jlsHash(s)
                        && jlsHash(s) == jlsHashFromArray(s)));
    }

    public static void main(String[] args) {
        // Latin-1 only: the side that already agreed.
        report("ASCII      ", "Hello, World");
        report("LATIN1-HIGH", u(0x61, 0xE9, 0xFF));
        // UTF-16: the side that diverged.
        report("GREEK      ", u(0x03A3, 0x039F, 0x03A3));
        report("TURKISH    ", u('T', 0x0130, 'T', 'L', 'E', 0x0131));
        report("SUPPLEMENT ", u('a', 'b', 0xD801, 0xDC01, 'c', 'd'));
        report("LONE-SURR  ", u('x', 0xD801, 'y'));
        report("ONE-UTF16  ", u(0x0100));
        report("EMPTY      ", "");

        // Second call: the JDK caches in `String.hash`, so a broken cache shows
        // up as a different answer the second time.
        System.out.println("--- second call (cache) ---");
        report("GREEK      ", u(0x03A3, 0x039F, 0x03A3));

        // The same content reached two ways must hash the same.
        String viaChars = u(0x03A3, 0x039F, 0x03A3);
        String viaConcat = u(0x03A3) + u(0x039F) + u(0x03A3);
        String viaSub = u('x', 0x03A3, 0x039F, 0x03A3).substring(1);
        System.out.println("SAME-CONTENT equalsA=" + viaChars.equals(viaConcat)
                + " equalsB=" + viaChars.equals(viaSub)
                + " hA=" + viaChars.hashCode()
                + " hB=" + viaConcat.hashCode()
                + " hC=" + viaSub.hashCode());

        // Bounds behaviour on `substring`: which exception class, from where.
        try {
            "Hello, World".substring(-1);
        } catch (Throwable t) {
            System.out.println("substring(-1) -> " + t.getClass().getName()
                    + " msg=" + t.getMessage());
        }
        try {
            "Hello, World".substring(3, 2);
        } catch (Throwable t) {
            System.out.println("substring(3,2) -> " + t.getClass().getName()
                    + " msg=" + t.getMessage());
        }
        try {
            "Hello, World".charAt(-1);
        } catch (Throwable t) {
            System.out.println("charAt(-1) -> " + t.getClass().getName()
                    + " msg=" + t.getMessage());
        }
        System.out.println("PROBE-DONE");
    }
}
