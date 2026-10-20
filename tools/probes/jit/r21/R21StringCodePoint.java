import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

/**
 * JIT round 21, wave 1, lane strings: code points, the Latin-1 / UTF-16 coder
 * switch, and the Latin-1 case operations.
 *
 * Run (both modes, JIT on; then the arms below):
 *   cratonvm -Xmx512m R21StringCodePoint            (--jdk-only, the launcher default)
 *   cratonvm --compatible -Xmx512m R21StringCodePoint
 *   java -Xmx512m R21StringCodePoint                (HotSpot 25: the reference)
 * Optional: -Dr21.reps=N (timing reps, default 9), -Dr21.rows=a,b (only these
 * time rows; facts always print).
 *
 * Arms (env): default; CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX=1 (CP21-1: the
 * codePointAt prefix, moves "time cp-iterate-*" and lets the loop methods reach
 * the optimizing tier); CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT=1 (STRINGS21-1:
 * coder()/isLatin1()/new String(byte[],byte) admitted, moves lower-latin1 and
 * cmpci-order); CRATONVM_JIT_IR_STRING_COMPARE_TO_CI_PREFIX=1 (STRINGS21-2:
 * cmpci-latin1); CRATONVM_JIT_IR_STRING_LOWER_UNCHANGED_PREFIX=1 (ZCP-3 half:
 * lower-latin1-unchanged, and since wave 3 lower-latin1 too: changed strings
 * are built inline); CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX=1 +
 * CRATONVM_JIT_IR_SB_LENGTH_PREFIX=1 (STRINGS21-3: releases the append kernel
 * from the single-pass pin, moves cp-append-*); all five together;
 * CRATONVM_JIT_IR_STRING_SEARCH_PREFIX=0
 * (every String prefix off: the control); CRATONVM_JIT=0 / --nojit (interpreter
 * control).
 *
 * HotSpot expectation: every line NOT starting with "time " is identical to
 * HotSpot 25's in every arm and both modes (code points, unpaired surrogates,
 * exception classes and messages, the case-fold checksums). Timing lines are
 * "time <row> <N> ns/op", the median of the reps after a warm-up; HotSpot 25
 * (w11d) for the handoff rows: cp-iterate ~52 per 41 code points, cp-append
 * ~25, tochars ~23, lower-latin1 ~52, cmpci-order ~12.
 */
public class R21StringCodePoint {
    // A mixed UTF-16 text: BMP, supplementary pairs, an unpaired high at the
    // end, an unpaired low, a reversed pair.
    static final String MIXED = "A\uD83D\uDE00b\uDE00c\uD800d\uDBFF\uDFFF\uDC00\uD800e\u00E9\u0100\uD83D";
    static final String LATIN = "Hello, W\u00F6rld \u00FF\u00E9\u00B5!";
    // ~49 units for the timing loops (the R20PIRCodePoint cp-iterate shape).
    static final String TEXT;
    static final String TEXT_LATIN;
    static {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < 7; i++) b.append("ab\uD835\uDC00cd\u00E9");
        TEXT = b.toString();
        StringBuilder l = new StringBuilder();
        for (int i = 0; i < 7; i++) l.append("abcd\u00E9fg");
        TEXT_LATIN = l.toString();
    }
    static final String[] WORDS = {
        "Hello", "WORLD", "MiXeD", "Caf\u00C9", "\u00C0LPHA", "Zebra", "QuIcK", "BROWN"
    };
    static final String[] WORDS_LOWER = {
        "hello", "world", "mixed", "caf\u00E9", "\u00E0lpha", "zebra", "quick", "brown"
    };
    static final String[] CI_A = {"hello", "WORLD", "Caf\u00E9", "\u00B5x", "\u00FFy", "abc", "Zeta"};
    static final String[] CI_B = {"HELLO", "world", "CAF\u00C9", "\u00B5X", "\u00FFY", "abd", "zetA"};

    static volatile int sink;

    static String hex(String s) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) b.append(' ');
            b.append(Integer.toHexString(s.charAt(i)));
        }
        return b.toString();
    }

    static String thrown(Runnable r) {
        try {
            r.run();
            return "no throw";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    // ---- kernels (the shapes the JIT compiles) ----

    static int iterate(String s) {
        int sum = 0;
        for (int i = 0; i < s.length(); ) {
            int cp = s.codePointAt(i);
            sum += cp;
            i += Character.charCount(cp);
        }
        return sum;
    }

    static int stream(String s) {
        return s.codePoints().sum();
    }

    static int append(StringBuilder sb, String s) {
        sb.setLength(0);
        for (int i = 0; i < s.length(); ) {
            int cp = s.codePointAt(i);
            sb.appendCodePoint(cp);
            i += Character.charCount(cp);
        }
        return sb.length();
    }

    static int toChars(int[] cps) {
        int sum = 0;
        for (int cp : cps) {
            char[] c = Character.toChars(cp);
            sum += c.length + c[0];
        }
        return sum;
    }

    static int lower(String[] words) {
        int h = 0;
        for (String w : words) h += w.toLowerCase(Locale.ROOT).hashCode();
        return h;
    }

    static int cmpCi(String[] a, String[] b) {
        int s = 0;
        for (int i = 0; i < a.length; i++) s += a[i].compareToIgnoreCase(b[i]);
        return s;
    }

    static int cmpOrder(String[] a, String[] b) {
        int s = 0;
        for (int i = 0; i < a.length; i++) s += String.CASE_INSENSITIVE_ORDER.compare(a[i], b[i]);
        return s;
    }

    // ---- facts ----

    static void facts() {
        for (String s : new String[] {MIXED, LATIN, ""}) {
            StringBuilder at = new StringBuilder();
            for (int i = 0; i < s.length(); i++) {
                if (i > 0) at.append(',');
                at.append(Integer.toHexString(s.codePointAt(i)));
            }
            System.out.println("fact codePointAt [" + hex(s) + "] -> " + at);
            StringBuilder before = new StringBuilder();
            for (int i = 1; i <= s.length(); i++) {
                if (i > 1) before.append(',');
                before.append(Integer.toHexString(s.codePointBefore(i)));
            }
            System.out.println("fact codePointBefore -> " + before);
            System.out.println("fact codePointCount " + s.codePointCount(0, s.length()));
            System.out.println("fact iterate " + iterate(s) + " stream " + stream(s));
            int[] cps = s.codePoints().toArray();
            System.out.println("fact codePoints " + Arrays.toString(cps));
            System.out.println("fact roundtrip " + new String(cps, 0, cps.length).equals(s));
            final String fs = s;
            System.out.println("fact at(-1) " + thrown(() -> fs.codePointAt(-1)));
            System.out.println("fact at(len) " + thrown(() -> fs.codePointAt(fs.length())));
            System.out.println("fact before(0) " + thrown(() -> fs.codePointBefore(0)));
            System.out.println("fact count(1,0) " + thrown(() -> fs.codePointCount(1, 0)));
            System.out.println("fact offset(0,99) " + thrown(() -> fs.offsetByCodePoints(0, 99)));
        }
        System.out.println("fact offset " + MIXED.offsetByCodePoints(0, 3) + " "
                + MIXED.offsetByCodePoints(MIXED.length(), -3));

        // A builder that starts LATIN1 and switches coder mid-stream.
        StringBuilder sb = new StringBuilder(4);
        sb.appendCodePoint('a').appendCodePoint(0xE9).appendCodePoint(0xFF);
        System.out.println("fact sb latin1 [" + hex(sb.toString()) + "] len " + sb.length());
        sb.appendCodePoint(0x100);
        System.out.println("fact sb inflated [" + hex(sb.toString()) + "] len " + sb.length());
        sb.appendCodePoint(0x1F600).appendCodePoint(0xD800).appendCodePoint(0xDC00).appendCodePoint('z');
        System.out.println("fact sb mixed [" + hex(sb.toString()) + "] len " + sb.length()
                + " cpcount " + sb.codePointCount(0, sb.length()));
        System.out.println("fact sb codePointAt " + Integer.toHexString(sb.codePointAt(4))
                + " " + Integer.toHexString(sb.codePointAt(5)) + " " + Integer.toHexString(sb.codePointAt(6)));
        final StringBuilder fsb = sb;
        System.out.println("fact sb append(-1) " + thrown(() -> fsb.appendCodePoint(-1)));
        System.out.println("fact sb append(0x110000) " + thrown(() -> fsb.appendCodePoint(0x110000)));
        System.out.println("fact sb at(len) " + thrown(() -> fsb.codePointAt(fsb.length())));
        sb.setLength(0);
        sb.appendCodePoint('q').appendCodePoint(0xE9);
        String after = sb.toString();
        System.out.println("fact sb reset [" + hex(after) + "] equals " + after.equals("q\u00E9")
                + " hash " + after.hashCode());
        StringBuffer sf = new StringBuffer();
        sf.appendCodePoint(0x10FFFF).appendCodePoint(0x41);
        System.out.println("fact sbuf [" + hex(sf.toString()) + "]");
        StringBuilder grow = new StringBuilder(0);
        for (int cp : new int[] {0x41, 0x1F600, 0xE9, 0x10000, 0xFFFF, 0x0}) grow.appendCodePoint(cp);
        System.out.println("fact sb grow [" + hex(grow.toString()) + "]");
        System.out.println("fact sb append-loop " + append(new StringBuilder(), MIXED) + " "
                + append(new StringBuilder(), LATIN));

        // Character.toChars.
        for (int cp : new int[] {0, 0x41, 0xFF, 0xD800, 0xFFFF, 0x10000, 0x1F600, 0x10FFFF}) {
            char[] c = Character.toChars(cp);
            System.out.println("fact toChars " + Integer.toHexString(cp) + " -> " + hex(new String(c)));
        }
        for (int cp : new int[] {-1, 0x110000, Integer.MIN_VALUE}) {
            final int fcp = cp;
            System.out.println("fact toChars(" + cp + ") " + thrown(() -> Character.toChars(fcp)));
        }
        System.out.println("fact toChars-loop " + toChars(new int[] {0x41, 0x1F600, 0xE9, 0x10FFFF}));

        // toLowerCase / toUpperCase: Turkish and Azeri dotless/dotted i, ROOT,
        // the LATIN1 -> UTF16 switch inside the Turkish arm, final sigma.
        String[] cases = {"TITLE", "I\u0130i\u0131", "\u00C0\u00DF\u00FF\u00B5", "\u03A3\u0391\u03A3 \u03A3",
            "MiXeD123", "abc", "\u00C9COLE"};
        Locale[] locales = {Locale.ROOT, Locale.forLanguageTag("tr"), Locale.forLanguageTag("az"),
            Locale.forLanguageTag("lt"), Locale.ENGLISH};
        for (String s : cases) {
            for (Locale loc : locales) {
                String lo = s.toLowerCase(loc);
                String up = s.toUpperCase(loc);
                System.out.println("fact case [" + hex(s) + "] " + loc.toLanguageTag() + " lower [" + hex(lo)
                        + "] upper [" + hex(up) + "] same " + (lo == s));
            }
        }
        System.out.println("fact lower(null) " + thrown(() -> "ABC".toLowerCase((Locale) null)));
        System.out.println("fact lower(null) unchanged " + thrown(() -> "abc".toLowerCase((Locale) null)));
        System.out.println("fact lower-unchanged-identity " + ("abc".toLowerCase(Locale.ROOT) == "abc")
                + " " + ("abc".toLowerCase() == "abc"));
        System.out.println("fact lower-loop " + lower(WORDS));
        System.out.println("fact lower-loop-unchanged " + lower(WORDS_LOWER));

        // compareToIgnoreCase across coders, the Latin-1 bytes whose upper case
        // leaves Latin-1, and the exact differences HotSpot returns.
        String[][] pairs = {
            {"abc", "ABC"}, {"abc", "ABD"}, {"abc", "ab"}, {"\u00B5", "\u039C"}, {"\u00B5", "\u03BC"},
            {"\u00FF", "\u0178"}, {"\u00DF", "SS"}, {"\u00E9", "\u00C9"}, {"abc", "ABC\u0100"},
            {"ABC\u0100", "abc"}, {"\u0130", "i"}, {"\u0131", "I"}, {"a\uD801\uDC00", "A\uD801\uDC28"},
            {"x\uD800", "X\uD800"}, {"[", "a"}, {"_", "A"}, {"", ""}, {"\u00D7", "\u00F7"},
            {"\u00B5", "\u00FF"}, {"\u00B5x", "\u00B5X"}
        };
        for (String[] p : pairs) {
            System.out.println("fact cmpci [" + hex(p[0]) + "] [" + hex(p[1]) + "] "
                    + p[0].compareToIgnoreCase(p[1]) + " order " + String.CASE_INSENSITIVE_ORDER.compare(p[0], p[1])
                    + " eqci " + p[0].equalsIgnoreCase(p[1]) + " rm " + p[0].regionMatches(true, 0, p[1], 0, 1));
        }
        System.out.println("fact cmpci(null) " + thrown(() -> "a".compareToIgnoreCase(null)));
        // Every pair of Latin-1 singletons: a checksum of the fold, both
        // directions, plus the per-char case maps.
        long acc = 0;
        for (int x = 0; x < 256; x++) {
            String sx = String.valueOf((char) x);
            for (int y = 0; y < 256; y++) {
                String sy = String.valueOf((char) y);
                acc = acc * 31 + sx.compareToIgnoreCase(sy);
                if (sx.equalsIgnoreCase(sy)) acc ^= (x << 8) | y;
            }
        }
        System.out.println("fact latin1-fold-checksum " + acc);
        long cm = 0;
        for (int x = 0; x < 256; x++) {
            String sx = String.valueOf((char) x);
            cm = cm * 131 + sx.toLowerCase(Locale.ROOT).hashCode();
            cm = cm * 131 + sx.toUpperCase(Locale.ROOT).hashCode();
            cm = cm * 131 + sx.toLowerCase(Locale.forLanguageTag("tr")).hashCode();
            cm = cm * 131 + (sx.toLowerCase(Locale.ROOT) == sx ? 1 : 0);
        }
        System.out.println("fact latin1-case-checksum " + cm);
        System.out.println("fact cmpci-loop " + cmpCi(CI_A, CI_B) + " order-loop " + cmpOrder(CI_A, CI_B));
    }

    // ---- timing ----

    interface Op { int run(); }

    static List<String> wanted;

    static void time(String row, int inner, Op op) {
        if (wanted != null && !wanted.contains(row)) return;
        int reps = Integer.getInteger("r21.reps", 9);
        if (reps < 7) reps = 7;
        // Warm-up: enough calls for both tiers.
        for (int w = 0; w < 20; w++) for (int i = 0; i < inner; i++) sink += op.run();
        long[] ns = new long[reps];
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < inner; i++) sink += op.run();
            ns[r] = (System.nanoTime() - t0) / inner;
        }
        Arrays.sort(ns);
        System.out.println("time " + row + " " + ns[reps / 2] + " ns/op");
    }

    public static void main(String[] args) {
        String rows = System.getProperty("r21.rows");
        if (rows != null && !rows.isEmpty()) wanted = new ArrayList<>(Arrays.asList(rows.split(",")));
        facts();
        final StringBuilder sb = new StringBuilder();
        final int[] cps = {0x41, 0x1F600, 0xE9, 0x10FFFF, 0x62, 0x20AC, 0x1D400, 0x63, 0x64, 0x10000, 0x65, 0x66,
            0x67, 0x2603, 0x68};
        time("cp-iterate-utf16", 20000, () -> iterate(TEXT));
        time("cp-iterate-latin1", 20000, () -> iterate(TEXT_LATIN));
        time("cp-stream-utf16", 20000, () -> stream(TEXT));
        time("cp-stream-latin1", 20000, () -> stream(TEXT_LATIN));
        time("cp-append-utf16", 20000, () -> append(sb, TEXT));
        time("cp-append-latin1", 20000, () -> append(sb, TEXT_LATIN));
        time("cp-tochars", 20000, () -> toChars(cps));
        time("lower-latin1", 20000, () -> lower(WORDS));
        time("lower-latin1-unchanged", 20000, () -> lower(WORDS_LOWER));
        time("cmpci-latin1", 20000, () -> cmpCi(CI_A, CI_B));
        time("cmpci-order", 20000, () -> cmpOrder(CI_A, CI_B));
        System.out.println("done");
    }
}
