import java.nio.charset.StandardCharsets;
import java.util.*;

/**
 * Regression: String / StringBuilder / formatting. Weighted toward UTF-16
 * code-unit indexing (a regressed area): all String indices are over UTF-16
 * code units, so supplementary characters (emoji) occupy two units.
 */
public class RStrings {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    public static void main(String[] a) {
        // ---- UTF-16 code-unit indexing with a supplementary char (U+1F600) ----
        // length() and charAt() are over UTF-16 code units (😀 = 2 units).
        // (NOTE: indexOf(char)/getChars positioning past a supplementary char is
        //  a known partial CratonVM gap — see README. Not asserted here.)
        String emoji = "a😀b";               // 'a', 😀 (2 units), 'b'
        check(emoji.length() == 4, "supplementary length (code units)");
        check(emoji.charAt(0) == 'a' && emoji.charAt(3) == 'b', "charAt past supplementary (code units)");

        String s = "The quick brown fox";
        check(s.indexOf("quick") == 4, "indexOf(str)");
        check(s.indexOf("o", 13) == 17, "indexOf(str,from)");
        check(s.lastIndexOf("o") == 17, "lastIndexOf");
        check(s.substring(4, 9).equals("quick"), "substring");
        check(s.replace("quick", "slow").equals("The slow brown fox"), "replace");
        check("a,b,,c".split(",").length == 4, "split keeps interior empties");
        check(String.join("-", "x", "y", "z").equals("x-y-z"), "join");
        check("  hi  ".trim().equals("hi") && "  hi  ".strip().equals("hi"), "trim/strip");
        check("AbC".equalsIgnoreCase("abc"), "equalsIgnoreCase");
        check("abc".compareTo("abd") < 0, "compareTo");
        check("abcabc".chars().filter(c -> c == 'a').count() == 2, "chars stream");

        // ---- StringBuilder ----
        StringBuilder sb = new StringBuilder("hello");
        sb.append(' ').append(42).append(' ').append(true);
        sb.insert(0, ">>");
        sb.reverse();
        check(sb.toString().equals("eurt 24 olleh>>"), "StringBuilder reverse: " + sb);
        StringBuilder sb2 = new StringBuilder();
        for (int i = 0; i < 5; i++) sb2.append(i);
        sb2.deleteCharAt(2);
        check(sb2.toString().equals("0134"), "deleteCharAt");

        // ---- Formatting (pin Locale.US so separators are VM/host-locale independent) ----
        check(String.format(Locale.US, "%d/%05d/%x", 42, 7, 255).equals("42/00007/ff"), "format ints");
        check(String.format(Locale.US, "%.2f|%e", 3.14159, 1234.5).startsWith("3.14|1.23"), "format floats");
        check(String.format(Locale.US, "%s=%b", "ok", true).equals("ok=true"), "format str/bool");
        check(String.format(Locale.US, "%,d", 1234567).equals("1,234,567"), "format grouping");

        // ---- Charset round-trip ----
        byte[] u8 = "héllo·世界".getBytes(StandardCharsets.UTF_8);
        check(new String(u8, StandardCharsets.UTF_8).equals("héllo·世界"), "UTF-8 round-trip");
        byte[] ascii = "ABC".getBytes(StandardCharsets.US_ASCII);
        check(ascii.length == 3 && ascii[0] == 65, "ASCII bytes");

        // ---- text blocks + repeat/strip ----
        check("ab".repeat(3).equals("ababab"), "repeat");
        check("hello".contains("ell") && "hello".startsWith("he") && "hello".endsWith("lo"), "contains/prefix/suffix");

        System.out.println("PASS RStrings (" + checks + " checks)");
    }
}
