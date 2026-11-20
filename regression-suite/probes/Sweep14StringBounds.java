import java.util.*;

/** Sweep 14: String bounds messages, case mapping, and locale-sensitive edges. */
public class Sweep14StringBounds {
    interface C { Object g() throws Exception; }
    static void t(String l, C c) {
        try { System.out.println("S " + l + " = " + c.g()); }
        catch (Throwable x) {
            System.out.println("S " + l + " = " + x.getClass().getName() + " | " + x.getMessage());
        }
    }
    static String u(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) b.append(',');
            b.append(Integer.toHexString(s.charAt(i)));
        }
        return b.toString();
    }

    public static void main(String[] a) {
        String s = "hello";

        // ---- bounds messages ---------------------------------------------------
        t("charAt_past", () -> s.charAt(9));
        t("charAt_negative", () -> s.charAt(-1));
        t("substring_begin_past", () -> s.substring(9));
        t("substring_begin_neg", () -> s.substring(-1));
        t("substring_end_past", () -> s.substring(1, 9));
        t("substring_end_lt_begin", () -> s.substring(3, 1));
        t("substring_empty_ok", () -> "[" + s.substring(5) + "]");
        t("codePointAt_past", () -> s.codePointAt(9));
        t("getChars_bad", () -> { s.getChars(0, 9, new char[9], 0); return "ok"; });
        t("indexOf_past_ok", () -> s.indexOf("l", 99));
        t("chars_at_negative_ok", () -> s.indexOf("l", -5));
        t("repeat_negative", () -> s.repeat(-1));
        t("strip_ok", () -> "[" + "  x  ".strip() + "]");
        t("split_limit_negative_ok", () -> Arrays.toString("a,b,,".split(",", -1)));
        t("split_limit_zero_ok", () -> Arrays.toString("a,b,,".split(",", 0)));
        t("valueOf_charArr_bad", () -> String.valueOf(new char[2], 1, 5));
        t("copyValueOf_bad", () -> String.copyValueOf(new char[2], -1, 1));
        t("new_String_bad_range", () -> new String(new char[2], 1, 5));
        t("new_String_bytes_bad", () -> new String(new byte[2], 1, 5));

        // ---- StringBuilder bounds ------------------------------------------------
        t("sb_charAt_past", () -> new StringBuilder("ab").charAt(5));
        t("sb_setCharAt_past", () -> { new StringBuilder("ab").setCharAt(5, 'x'); return "ok"; });
        t("sb_deleteCharAt_past", () -> new StringBuilder("ab").deleteCharAt(5));
        t("sb_insert_past", () -> new StringBuilder("ab").insert(5, "x"));
        t("sb_substring_past", () -> new StringBuilder("ab").substring(5));
        t("sb_setLength_negative", () -> { new StringBuilder("ab").setLength(-1); return "ok"; });
        t("sb_delete_end_past_ok", () -> new StringBuilder("ab").delete(1, 99).toString());

        // ---- case mapping, including the ones that change LENGTH ------------------
        t("upper_sharp_s", () -> u("ß".toUpperCase(Locale.ROOT)));
        t("upper_ligature_fi", () -> u("ﬁ".toUpperCase(Locale.ROOT)));
        t("lower_sigma_final", () -> u("ΣΣ".toLowerCase(Locale.ROOT)));
        t("upper_turkish_i", () -> u("i".toUpperCase(new Locale("tr"))));
        t("lower_turkish_I", () -> u("I".toLowerCase(new Locale("tr"))));
        t("upper_root_i", () -> u("i".toUpperCase(Locale.ROOT)));
        t("title_dz", () -> u("ǳ".toUpperCase(Locale.ROOT)));
        t("upper_surrogate_pair", () -> u("𐐀".toUpperCase(Locale.ROOT)));
        t("lower_lone_surrogate", () -> u(("a" + '\ud800' + "b").toLowerCase(Locale.ROOT)));
        t("upper_lone_surrogate", () -> u(("a" + '\ud800' + "b").toUpperCase(Locale.ROOT)));

        // ---- equality and comparison ----------------------------------------------
        t("equalsIgnoreCase_turkish", () -> "I".equalsIgnoreCase("i"));
        t("compareToIgnoreCase", () -> Integer.signum("A".compareToIgnoreCase("a")));
        t("compareTo_length", () -> "ab".compareTo("abc"));
        t("compareTo_char", () -> "b".compareTo("a"));
        t("regionMatches_ic", () -> "ABC".regionMatches(true, 0, "abc", 0, 3));
        t("regionMatches_oob_ok", () -> "ABC".regionMatches(0, "abc", 0, 99));
        t("contentEquals_sb", () -> "ab".contentEquals(new StringBuilder("ab")));
        t("intern_identity", () -> ("he" + "llo").intern() == "hello");
    }
}
