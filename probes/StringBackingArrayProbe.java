import java.lang.reflect.Field;

/**
 * Which of `String.hashCode`'s three inputs is wrong?
 *
 * `probes/StringUtf16HashProbe` established that for a UTF-16 string
 * `s.hashCode()` disagrees with the JLS fold over `s.charAt(i)` on the SAME
 * object. Both go through `StringUTF16.getChar` — `charAt` via
 * `StringUTF16.charAt`, `hashCode` via `StringUTF16.hashCode` — so "getChar is
 * broken" cannot be the whole story: one caller gets the right answer.
 *
 * This probe reads the two fields those callers actually consume, `value` and
 * `coder`, so the remaining possibilities can be told apart instead of guessed:
 *
 *   * `coder` wrong  -> `isLatin1()` picks the wrong branch, and the byte
 *     count/stride follow from that;
 *   * `value` wrong  -> the bytes are not what `charAt` implies they are, and
 *     `charAt` is compensating somewhere else;
 *   * both right     -> the fault is in `StringUTF16.hashCode`'s own loop,
 *     which is the only thing left.
 *
 * It then computes, in plain Java from those raw bytes, each candidate reading
 * a broken implementation could plausibly be doing, and reports which one
 * `hashCode()` matches. Naming the exact wrong input is what turns this from a
 * bug report into a fix.
 *
 * Reflection into `java.lang.String` needs `--add-opens java.base/java.lang=ALL-UNNAMED`
 * on HotSpot. If it is denied the probe says so and still prints the hashes,
 * which is enough to compare the two VMs.
 */
public class StringBackingArrayProbe {

    static String u(int... units) {
        char[] c = new char[units.length];
        for (int i = 0; i < units.length; i++) {
            c[i] = (char) units[i];
        }
        return new String(c);
    }

    static int fold(int[] units) {
        int h = 0;
        for (int x : units) {
            h = 31 * h + x;
        }
        return h;
    }

    static int jlsHash(String s) {
        int h = 0;
        for (int i = 0; i < s.length(); i++) {
            h = 31 * h + s.charAt(i);
        }
        return h;
    }

    static void report(String tag, String s) {
        int actual = s.hashCode();
        int jls = jlsHash(s);
        System.out.println(tag + " len=" + s.length()
                + " hashCode=" + actual + " jls=" + jls
                + " agree=" + (actual == jls));

        byte[] value = null;
        int coder = -1;
        try {
            Field vf = String.class.getDeclaredField("value");
            vf.setAccessible(true);
            value = (byte[]) vf.get(s);
            Field cf = String.class.getDeclaredField("coder");
            cf.setAccessible(true);
            coder = ((Byte) cf.get(s)).intValue();
        } catch (Throwable t) {
            System.out.println("   fields unavailable: " + t.getClass().getName());
            return;
        }

        StringBuilder raw = new StringBuilder();
        for (byte b : value) {
            raw.append(String.format("%02X ", b & 0xff));
        }
        System.out.println("   coder=" + coder + " value.length=" + value.length
                + " bytes=[" + raw.toString().trim() + "]");

        int n = s.length();
        // Every reading a broken loop could be doing, computed here in Java.
        int[][] candidates = new int[6][];
        String[] names = {
            "code-units-LE (correct)",
            "first-n-bytes-masked",
            "first-n-bytes-SIGN-EXTENDED",
            "all-bytes-masked",
            "code-units-BE",
            "or-of-byte-pairs",
        };
        candidates[0] = new int[n];
        candidates[1] = new int[Math.min(n, value.length)];
        candidates[2] = new int[Math.min(n, value.length)];
        candidates[3] = new int[value.length];
        candidates[4] = new int[n];
        candidates[5] = new int[n];
        for (int i = 0; i < n && 2 * i + 1 < value.length; i++) {
            int lo = value[2 * i] & 0xff;
            int hi = value[2 * i + 1] & 0xff;
            candidates[0][i] = (hi << 8) | lo;
            candidates[4][i] = (lo << 8) | hi;
            candidates[5][i] = lo | hi;
        }
        for (int i = 0; i < candidates[1].length; i++) {
            candidates[1][i] = value[i] & 0xff;
            candidates[2][i] = (char) value[i];
        }
        for (int i = 0; i < value.length; i++) {
            candidates[3][i] = value[i] & 0xff;
        }
        for (int i = 0; i < candidates.length; i++) {
            int h = fold(candidates[i]);
            System.out.println("   " + (h == actual ? "MATCH  " : "       ")
                    + names[i] + " = " + h);
        }
    }

    public static void main(String[] args) {
        report("ASCII  ", "Hello, World");
        report("GREEK  ", u(0x03A3, 0x039F, 0x03A3));
        report("TURKISH", u('T', 0x0130, 'T', 'L', 'E', 0x0131));
        report("SUPP   ", u('a', 'b', 0xD801, 0xDC01, 'c', 'd'));
        report("ONEUTF ", u(0x0100));
        System.out.println("PROBE-DONE");
    }
}
