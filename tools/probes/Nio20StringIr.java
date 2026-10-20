import java.util.*;
import java.util.function.Supplier;

/**
 * nio20 wave 3, lane L2: the optimizing tier's inline String search prefix
 * (`String.hashCode` / `equals` / `compareTo` / `indexOf(int)` / `indexOf(String)`), which lets a
 * method calling one of them reach the optimizing tier without losing the inline body
 * (`CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`, default ON; `=0` restores the single-pass pin and the
 * planning-gate bail for those methods).
 *
 * Bench rows print `<row> <N> ns/op`; the `conf` row prints deterministic facts to diff against
 * HotSpot 25. `-Dnio20.rows=a,b` selects rows (default: all). `-Dnio20.ops=N` sets the bench op
 * count (default 20_000_000).
 *
 *   hash       String.hashCode() over cached keys (the prefix's cached-hash read)
 *   equals     String.equals(Object), half equal / half last-byte-different 24-char keys
 *   compareto  String.compareTo(String) over LATIN1 keys sharing a 16-char prefix
 *   indexofc   String.indexOf(int) on a 64-char LATIN1 haystack, variable needle
 *   indexofs   String.indexOf(String) on a 64-char LATIN1 haystack
 *   mixed      equals + substring in one method (moves only with CRATONVM_JIT_IR_STRING_CALL_ADMIT=1)
 *   conf       answers of all five members over LATIN1 / UTF16 / edge inputs, after warm-up
 *
 * nio20 wave 4, lane L5 rows (A/B arms: default vs CRATONVM_JIT_IR_STRING_SSE2=0 for the long
 * rows, vs CRATONVM_JIT_IR_STRING_SEARCH_PREFIX=0 for the UTF16 rows, vs
 * CRATONVM_JIT_IR_STRING_HASH_COMPUTE=0 for hashfresh, vs CRATONVM_JIT_IR_SB_PREFIX=0 for sb*):
 *   equals64   String.equals over 64-char keys (last char differs for odd i)
 *   equals256  the same over 256-char keys
 *   indexofc256 String.indexOf(int) on a 256-char LATIN1 haystack (late / absent needles)
 *   compareto16 String.compareTo over UTF16 keys sharing a 16-char prefix
 *   indexofc16 String.indexOf(int) on a 64-char UTF16 haystack
 *   indexofs16 String.indexOf(String) UTF16 haystack, UTF16 needles
 *   indexofsul String.indexOf(String) UTF16 haystack, LATIN1 needles
 *   hashfresh  hashCode() of a fresh substring each op (the uncached hash)
 *   sbappend   StringBuilder.append(char) x16 + length() + charAt() per op, builder reused
 *   sbconf     (part of conf) StringBuilder length / charAt / append(char) answers and exceptions
 *   sbappendstr StringBuilder.append(String) x4 (LATIN1 pieces) + length() per op, builder reused
 *              (nio20 wave 5 L5; A/B CRATONVM_JIT_IR_SB_APPEND_STRING_PREFIX=0)
 *   sbstrconf  (part of conf) StringBuilder.append(String): LATIN1 / UTF16 / empty / null / growth
 *
 * nio20 wave 6, lane L3 rows (the w5c log shows the append(String) prefix IS emitted at
 * sbAppendStrLoop's site, `[ir] string-builder-prefix n107 .. kind=SbAppendString`, so these split
 * what else the sbappendstr op pays):
 *   sbappendstr4 the sbappendstr shape with the piece picked by `& 3` from a 4-element array: no
 *              `lrem` (sbappendstr's `(i + j) % PIECES.length` is a 64-bit IDIV per append unless
 *              the array length folds to a constant, which HotSpot's C2 does for a static final)
 *   lremonly   the four `(i + j) % PIECES.length` and array loads alone, summed (no builder)
 */
public class Nio20StringIr {
    static String rows = System.getProperty("nio20.rows", "all");
    static long ops = Long.getLong("nio20.ops", 20_000_000L);
    static boolean on(String r) { return rows.equals("all") || Arrays.asList(rows.split(",")).contains(r); }
    static volatile Object sink;

    static final String[] KEYS = new String[64];
    static final String[] KEYS2 = new String[64];
    static {
        for (int i = 0; i < KEYS.length; i++) {
            KEYS[i] = "key-prefix-0123456789-" + (char) ('a' + (i % 26)) + (i / 26);
            // Same content as KEYS[i] for even i (distinct object), last byte different for odd i.
            String base = "key-prefix-0123456789-" + (char) ('a' + (i % 26));
            KEYS2[i] = (i % 2 == 0) ? new String(base + (i / 26)) : base + (char) ('0' + (i / 26) + 5);
            KEYS[i].hashCode();
        }
    }
    static final String HAY = "the quick brown fox jumps over the lazy dog / 0123456789 ABCDEFG";
    static final String[] NEEDLES = {"fox", "lazy", "dog /", "9 AB", "cat", "the q", "EFG", "zz"};

    // nio20 wave 4 (L5) fixtures.
    static String rep(String s, int n) { StringBuilder b = new StringBuilder(); while (b.length() < n) b.append(s); return b.substring(0, n); }
    static final String[] K64 = new String[16], K64B = new String[16], K256 = new String[16], K256B = new String[16];
    static final String[] U16 = new String[64];
    static final String HAY256 = rep("abcdefghijklmnopqrstuvwxyz0123456789", 250) + "!?#$%&";
    static final String HAY16 = rep("фхцчшщъыьэюя ", 56) + "Ж abc ЖЖ";
    static final String[] NEEDLES16 = {"цчш", "Ж", "ЖЖ", "юя ф", "яя", "abc"};
    static final String[] NEEDLESUL = {"abc", " ", "c Ж", "zz", "b", "Ж abc"};
    static {
        for (int i = 0; i < 16; i++) {
            String p64 = rep("http://example.com/path/segment/", 63), p256 = rep("Content-Type: application/json; ", 255);
            K64[i] = p64 + (char) ('a' + i);
            K64B[i] = (i % 2 == 0) ? new String(K64[i]) : p64 + (char) ('A' + i);
            K256[i] = p256 + (char) ('a' + i);
            K256B[i] = (i % 2 == 0) ? new String(K256[i]) : p256 + (char) ('A' + i);
        }
        for (int i = 0; i < U16.length; i++) U16[i] = "ключ-префикс-0123-" + (char) ('а' + (i % 26)) + (i / 26);
    }

    // Each loop is its own method so it tiers up through the method-entry door as well as OSR.
    static long hashLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += KEYS[(int) (i & 63)].hashCode();
        return acc;
    }

    static long equalsLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            int k = (int) (i & 63);
            if (KEYS[k].equals(KEYS2[k])) acc++;
        }
        return acc;
    }

    static long compareLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += KEYS[(int) (i & 63)].compareTo(KEYS[(int) ((i + 7) & 63)]);
        return acc;
    }

    static long indexOfCharLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += HAY.indexOf('A' + (int) (i & 15));
        return acc;
    }

    static long indexOfStrLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += HAY.indexOf(NEEDLES[(int) (i & 7)]);
        return acc;
    }

    static long mixedLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            int k = (int) (i & 63);
            if (KEYS[k].equals(KEYS2[k])) acc += KEYS[k].substring(4, 10).length();
        }
        return acc;
    }

    static long equalsLong(String[] a, String[] b, long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            int k = (int) (i & 15);
            if (a[k].equals(b[k])) acc++;
        }
        return acc;
    }

    static long equals64Loop(long n) { return equalsLong(K64, K64B, n); }
    static long equals256Loop(long n) { return equalsLong(K256, K256B, n); }

    static long indexOfChar256Loop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += HAY256.indexOf("!?#$%&@".charAt((int) (i % 7)));
        return acc;
    }

    static long compare16Loop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += U16[(int) (i & 63)].compareTo(U16[(int) ((i + 7) & 63)]);
        return acc;
    }

    static long indexOfChar16Loop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += HAY16.indexOf('ф' + (int) (i & 15));
        return acc;
    }

    static long indexOfStr16Loop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += HAY16.indexOf(NEEDLES16[(int) (i % NEEDLES16.length)]);
        return acc;
    }

    static long indexOfStrULLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += HAY16.indexOf(NEEDLESUL[(int) (i % NEEDLESUL.length)]);
        return acc;
    }

    static long hashFreshLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) acc += KEYS[(int) (i & 63)].substring(1).hashCode();
        return acc;
    }

    static final StringBuilder SB = new StringBuilder(64);

    static long sbAppendLoop(long n) {
        long acc = 0;
        StringBuilder sb = SB;
        for (long i = 0; i < n; i++) {
            sb.setLength(0);
            for (int j = 0; j < 16; j++) sb.append((char) ('a' + ((i + j) & 15)));
            acc += sb.length() + sb.charAt((int) (i & 15));
        }
        return acc;
    }

    static final String[] PIECES = {"GET ", "/index.html", " HTTP/1.1", "\r\n", "", "Host: a"};

    static long sbAppendStrLoop(long n) {
        long acc = 0;
        StringBuilder sb = new StringBuilder(64);
        for (long i = 0; i < n; i++) {
            sb.setLength(0);
            for (int j = 0; j < 4; j++) sb.append(PIECES[(int) ((i + j) % PIECES.length)]);
            acc += sb.length();
        }
        return acc;
    }

    static final String[] PIECES4 = {"GET ", "/index.html", " HTTP/1.1", "Host: a"};

    // nio20 wave 6, lane L3: sbAppendStrLoop without the 64-bit remainder.
    static long sbAppendStr4Loop(long n) {
        long acc = 0;
        StringBuilder sb = new StringBuilder(64);
        String[] pieces = PIECES4;
        for (long i = 0; i < n; i++) {
            sb.setLength(0);
            int base = (int) i;
            for (int j = 0; j < 4; j++) sb.append(pieces[(base + j) & 3]);
            acc += sb.length();
        }
        return acc;
    }

    // nio20 wave 6, lane L3: what sbAppendStrLoop pays besides the builder.
    static long lremOnlyLoop(long n) {
        long acc = 0;
        for (long i = 0; i < n; i++) {
            for (int j = 0; j < 4; j++) acc += PIECES[(int) ((i + j) % PIECES.length)].length();
        }
        return acc;
    }

    static void bench(String row, long n, Supplier<Long> body) {
        sink = body.get();
        long t0 = System.nanoTime();
        sink = body.get();
        long t1 = System.nanoTime();
        System.out.println(row + " " + ((t1 - t0) / Math.max(1, n)) + " ns/op");
    }

    // Out-of-line so the conformance answers come from a compiled caller.
    static int eq(String a, Object b) { return a.equals(b) ? 1 : 0; }
    static int cmp(String a, String b) { return a.compareTo(b); }
    static int ixc(String a, int ch) { return a.indexOf(ch); }
    static int ixs(String a, String b) { return a.indexOf(b); }
    static int hc(String a) { return a.hashCode(); }

    static void conf() {
        String latin = "café latin one";
        String utf = "café фу utf sixteen";
        String supp = "x𐐷y";
        Object[] others = {null, new StringBuilder("café latin one"), "café latin one".toCharArray(), 42};
        String[] strs = {"", "a", "abc", "abd", "abcd", latin, new String(latin), utf, supp,
                "the quick brown fox jumps over", "the quick brown fox jumps overt", "ÿþ", "Ā"};
        int[] needles = {'a', 'c', 0xe9, 0x444, 0x10437, -1, 0x110000, 0xffff, 'z', 0};
        // Warm the five callers well past every tier threshold first.
        long warm = 0;
        for (int r = 0; r < 300_000; r++) {
            String a = strs[r % strs.length], b = strs[(r / 7) % strs.length];
            warm += eq(a, b) + cmp(a, b) + ixc(a, needles[r % needles.length]) + ixs(a, b) + hc(a);
            warm += eq(a, others[r % others.length]);
        }
        sink = warm;
        for (String a : strs) {
            StringBuilder sb = new StringBuilder("conf ").append(show(a)).append(" hash=").append(hc(a));
            for (String b : strs) {
                sb.append(" | ").append(show(b)).append(" eq=").append(eq(a, b))
                  .append(" cmp=").append(cmp(a, b)).append(" ix=").append(ixs(a, b));
            }
            for (int ch : needles) sb.append(" c").append(Integer.toHexString(ch)).append('=').append(ixc(a, ch));
            for (Object o : others) sb.append(" o=").append(eq(a, o));
            System.out.println(sb);
        }
        try {
            System.out.println("conf cmp-null " + cmp("a", null));
        } catch (NullPointerException e) {
            System.out.println("conf cmp-null NPE");
        }
        try {
            System.out.println("conf ixs-null " + ixs("a", null));
        } catch (NullPointerException e) {
            System.out.println("conf ixs-null NPE");
        }
        try {
            System.out.println("conf hc-null " + hc(null));
        } catch (NullPointerException e) {
            System.out.println("conf hc-null NPE");
        }
        conf4();
    }

    // nio20 wave 4 (L5): UTF16 / long / fresh-hash / StringBuilder facts, from compiled callers.
    static int sbLen(StringBuilder b) { return b.length(); }
    static char sbAt(StringBuilder b, int i) { return b.charAt(i); }
    static StringBuilder sbApp(StringBuilder b, char c) { return b.append(c); }
    static StringBuilder sbAppS(StringBuilder b, String s) { return b.append(s); }

    static void conf4() {
        String l256 = rep("abcdefghijklmnopqrstuvwxyz", 256);
        String u64 = rep("фхцчшщъыьэюя", 64);
        String[] strs = {"", "Ж", "ЖЖ", "ЖЖЖ", "фх", "хф", u64, u64 + "a", u64.substring(0, 40) + "Я" + u64.substring(41),
                l256, l256.substring(0, 200) + "!" + l256.substring(201), "abc", "abcЖ", "c Ж", "Жabc", "f5a5a608"};
        int[] needles = {'a', 'z', 'Ж', 'я', 0xd800, 0xffff, 0x10437, -1, 0x110000, 0};
        long warm = 0;
        for (int r = 0; r < 300_000; r++) {
            String a = strs[r % strs.length], b = strs[(r / 5) % strs.length];
            warm += eq(a, b) + cmp(a, b) + ixc(a, needles[r % needles.length]) + ixs(a, b) + hc(new String(a));
        }
        sink = warm;
        for (String a : strs) {
            StringBuilder sb = new StringBuilder("conf4 ").append(show(a.length() > 24 ? a.substring(0, 24) : a))
                .append(" len=").append(a.length()).append(" hash=").append(hc(new String(a)));
            for (String b : strs) sb.append(" | eq=").append(eq(a, b)).append(" cmp=").append(cmp(a, b)).append(" ix=").append(ixs(a, b));
            for (int ch : needles) sb.append(" c").append(Integer.toHexString(ch)).append('=').append(ixc(a, ch));
            System.out.println(sb);
        }
        // StringBuilder: warm the three helpers, then print answers over LATIN1, UTF16, growth and errors.
        long w2 = 0;
        for (int r = 0; r < 300_000; r++) {
            StringBuilder b = new StringBuilder(4);
            for (int j = 0; j < 6; j++) sbApp(b, (char) ('a' + j + (r % 3 == 0 && j == 4 ? 0x400 : 0)));
            w2 += sbLen(b) + sbAt(b, r % 6);
        }
        sink = w2;
        StringBuilder b = new StringBuilder(2);
        StringBuilder out = new StringBuilder("sbconf");
        char[] cs = {'a', 'b', 'c', 'ÿ', 'Ж', 'd', 0xd800, 'e'};
        for (char c : cs) {
            StringBuilder r = sbApp(b, c);
            out.append(" app=").append(r == b).append(':').append(sbLen(b));
        }
        for (int i = -1; i <= sbLen(b); i++) {
            try {
                out.append(" at").append(i).append('=').append(Integer.toHexString(sbAt(b, i)));
            } catch (RuntimeException e) {
                out.append(" at").append(i).append('=').append(e.getClass().getName()).append(':').append(e.getMessage());
            }
        }
        out.append(" str=").append(show(b.toString()));
        try {
            sbLen(null);
        } catch (NullPointerException e) {
            out.append(" len-null=NPE");
        }
        try {
            sbApp(null, 'x');
        } catch (NullPointerException e) {
            out.append(" app-null=NPE");
        }
        StringBuffer sbuf = new StringBuffer("buffer");
        out.append(" sbuf=").append(((CharSequence) sbuf).length());
        System.out.println(out);
        // nio20 wave 5 (L5): append(String) from a compiled caller, warmed on LATIN1 pieces,
        // then LATIN1 / empty / UTF16 / null / growth answers.
        long w3 = 0;
        for (int r = 0; r < 300_000; r++) {
            StringBuilder s = new StringBuilder(32);
            for (int j = 0; j < 4; j++) sbAppS(s, PIECES[(r + j) % PIECES.length]);
            w3 += s.length();
        }
        sink = w3;
        StringBuilder s = new StringBuilder(4);
        StringBuilder out2 = new StringBuilder("sbstrconf");
        String[] adds = {"ab", "", "c", "Жx", "de", null, "0123456789abcdef0123456789", "ÿ"};
        for (String add : adds) {
            StringBuilder r = sbAppS(s, add);
            out2.append(" app=").append(r == s).append(':').append(s.length());
        }
        out2.append(" str=").append(show(s.toString()));
        StringBuilder t = new StringBuilder(64);
        for (int j = 0; j < 8; j++) sbAppS(t, "xy");
        out2.append(" t=").append(show(t.toString())).append(':').append(t.capacity() >= 16);
        try {
            sbAppS(null, "x");
        } catch (NullPointerException e) {
            out2.append(" app-null=NPE");
        }
        System.out.println(out2);
    }

    static String show(String s) {
        StringBuilder sb = new StringBuilder("\"");
        for (char c : s.toCharArray()) {
            if (c >= 0x20 && c < 0x7f) sb.append(c); else sb.append("\\u").append(String.format("%04x", (int) c));
        }
        return sb.append('"').toString();
    }

    public static void main(String[] a) {
        if (on("conf")) conf();
        if (on("hash")) bench("hash", ops, () -> hashLoop(ops));
        if (on("equals")) bench("equals", ops, () -> equalsLoop(ops));
        if (on("compareto")) bench("compareto", ops, () -> compareLoop(ops));
        if (on("indexofc")) bench("indexofc", ops / 4, () -> indexOfCharLoop(ops / 4));
        if (on("indexofs")) bench("indexofs", ops / 4, () -> indexOfStrLoop(ops / 4));
        if (on("mixed")) bench("mixed", ops / 4, () -> mixedLoop(ops / 4));
        if (on("equals64")) bench("equals64", ops / 4, () -> equals64Loop(ops / 4));
        if (on("equals256")) bench("equals256", ops / 8, () -> equals256Loop(ops / 8));
        if (on("indexofc256")) bench("indexofc256", ops / 8, () -> indexOfChar256Loop(ops / 8));
        if (on("compareto16")) bench("compareto16", ops, () -> compare16Loop(ops));
        if (on("indexofc16")) bench("indexofc16", ops / 4, () -> indexOfChar16Loop(ops / 4));
        if (on("indexofs16")) bench("indexofs16", ops / 4, () -> indexOfStr16Loop(ops / 4));
        if (on("indexofsul")) bench("indexofsul", ops / 4, () -> indexOfStrULLoop(ops / 4));
        if (on("hashfresh")) bench("hashfresh", ops / 4, () -> hashFreshLoop(ops / 4));
        if (on("sbappend")) bench("sbappend", ops / 4, () -> sbAppendLoop(ops / 4));
        if (on("sbappendstr")) bench("sbappendstr", ops / 4, () -> sbAppendStrLoop(ops / 4));
        if (on("sbappendstr4")) bench("sbappendstr4", ops / 4, () -> sbAppendStr4Loop(ops / 4));
        if (on("lremonly")) bench("lremonly", ops / 4, () -> lremOnlyLoop(ops / 4));
        System.out.println("done");
    }
}
