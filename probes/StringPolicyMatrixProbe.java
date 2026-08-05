import java.io.UnsupportedEncodingException;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

/**
 * Matrix probe for the forced-native `java/lang/String` policy
 * (docs/known-issues/jdk-only/forced-native-string-policy-two-lists-that-disagree.md).
 *
 * Every `String` shape either half of that policy mentions is exercised over a
 * fixed corpus that deliberately includes the cases a layout-neutral native and
 * the real JDK bytecode are most likely to disagree about:
 *
 *   * surrogate pairs (`charAt` / `length` index by UTF-16 CODE UNIT, not by
 *     code point — the bug the `charAt` registration comment records);
 *   * lone surrogates, which are not valid Unicode scalar values and therefore
 *     cannot survive a round trip through a Rust `String`;
 *   * Turkish dotted/dotless I and the Greek final sigma, where
 *     `toLowerCase(Locale)` differs from `toLowerCase()`;
 *   * `trim` vs `strip` (`trim` cuts <= U+0020, `strip` cuts by
 *     `Character.isWhitespace`);
 *   * `compareTo` on strings whose difference is beyond the shorter length;
 *   * regex metacharacters through `replaceAll` / `replaceFirst` / `matches`
 *     and `$`/backslash escapes in the replacement;
 *   * the ERROR behaviour — every out-of-range index and every null argument is
 *     asserted by exception CLASS NAME, because "returns the same value" is
 *     only half a contract and the forced natives are exactly where an
 *     exception silently becomes a default value.
 *
 * The output is one line per case, `<n> <shape> => <value>`, and is meant to be
 * diffed byte-for-byte against a HotSpot run of the same class. It prints
 * VALUES, never "ok": a probe that prints "ok" cannot show a wrong number.
 *
 * Run it on HotSpot FIRST — the JDK is the contract, and several "obvious"
 * expectations in this file were written wrong and corrected by that run.
 */
public class StringPolicyMatrixProbe {

    static int n = 0;
    static final List<String> LINES = new ArrayList<>();

    static void emit(String shape, Object value) {
        n++;
        LINES.add(n + " " + shape + " => " + render(value));
    }

    static String render(Object v) {
        if (v == null) {
            return "null";
        }
        if (v instanceof String) {
            return "\"" + escape((String) v) + "\"";
        }
        if (v instanceof char[]) {
            return "\"" + escape(new String((char[]) v)) + "\"";
        }
        if (v instanceof String[]) {
            StringBuilder sb = new StringBuilder("[");
            String[] a = (String[]) v;
            for (int i = 0; i < a.length; i++) {
                if (i > 0) {
                    sb.append(", ");
                }
                sb.append("\"").append(escape(a[i])).append("\"");
            }
            return sb.append("]").toString();
        }
        if (v instanceof byte[]) {
            StringBuilder sb = new StringBuilder("bytes[");
            byte[] a = (byte[]) v;
            for (int i = 0; i < a.length; i++) {
                if (i > 0) {
                    sb.append(' ');
                }
                sb.append(String.format("%02x", a[i] & 0xff));
            }
            return sb.append("]").toString();
        }
        if (v instanceof Character) {
            return "U+" + String.format("%04X", (int) (Character) v);
        }
        return String.valueOf(v);
    }

    /** Escape to pure ASCII so the transcript is byte-identical on any console. */
    static String escape(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c == '"') {
                sb.append("\\\"");
            } else if (c == '\\') {
                sb.append("\\\\");
            } else if (c >= 0x20 && c < 0x7f) {
                sb.append(c);
            } else {
                sb.append(String.format("\\u%04X", (int) c));
            }
        }
        return sb.toString();
    }

    /**
     * Record a call that is EXPECTED to throw. Names the exception class and
     * the message, because the forced natives return defaults where the real
     * bytecode throws, and a value-only comparison cannot see that.
     */
    static void expectThrow(String shape, Thrower body) {
        n++;
        try {
            Object value = body.run();
            LINES.add(n + " " + shape + " => NO-THROW " + render(value));
        } catch (Throwable t) {
            LINES.add(n + " " + shape + " => throws " + t.getClass().getName()
                    + " msg=" + render(t.getMessage()));
        }
    }

    interface Thrower {
        Object run() throws Throwable;
    }

    // ---- corpus -----------------------------------------------------------
    // Built with explicit char values so the SOURCE FILE stays pure ASCII: a
    // harness that exports LC_ALL=C otherwise breaks javac's source encoding
    // and this file silently compiles to different constants.

    /** "ab" + U+10401 (DESERET CAPITAL LETTER LONG I) + "cd" — a surrogate pair. */
    static final String SUPP = "ab" + new String(new char[] { '\uD801', '\uDC01' }) + "cd";
    /** A LONE high surrogate: not a valid scalar value, cannot round-trip UTF-8. */
    static final String LONE = "x" + new String(new char[] { '\uD801' }) + "y";
    /** Turkish dotted capital I + dotless small i. */
    static final String TURKISH = new String(new char[] { 'T', '\u0130', 'T', 'L', 'E', '\u0131' });
    /** Greek sigma: final-sigma casing is position-dependent. */
    static final String GREEK = new String(new char[] { '\u03A3', '\u039F', '\u03A3' });
    /** Leading/trailing chars that `trim` cuts but `strip` treats differently. */
    static final String TRIMMY = new String(new char[] { '\u0001', ' ', 'a', 'b', ' ', '\u001f' });
    /** NBSP: `strip` does NOT cut it (not isWhitespace), `trim` does not either (>0x20). */
    static final String NBSP = new String(new char[] { '\u00A0', 'a', '\u00A0' });
    static final String EMPTY = "";
    static final String PLAIN = "Hello, World";
    static final String REPEAT = "abcabcabc";

    static final String[] CORPUS = { EMPTY, PLAIN, SUPP, LONE, TURKISH, GREEK, TRIMMY, NBSP, REPEAT };
    static final String[] CORPUS_NAMES = {
        "EMPTY", "PLAIN", "SUPP", "LONE", "TURKISH", "GREEK", "TRIMMY", "NBSP", "REPEAT"
    };

    public static void main(String[] args) throws Exception {
        lengthAndCharAt();
        isEmptyAndSubstring();
        startsEndsContainsIndexOf();
        equalsHashCodeCompare();
        caseAndTrim();
        concatReplaceSplit();
        regexFamily();
        charsetConstructors();
        pairedProperties();

        for (String line : LINES) {
            System.out.println(line);
        }
        System.out.println("STRING-MATRIX cases=" + n);
    }

    // ---- length / charAt --------------------------------------------------

    static void lengthAndCharAt() {
        for (int i = 0; i < CORPUS.length; i++) {
            String s = CORPUS[i];
            emit("length(" + CORPUS_NAMES[i] + ")", s.length());
            emit("isEmpty(" + CORPUS_NAMES[i] + ")", s.isEmpty());
            StringBuilder units = new StringBuilder();
            for (int k = 0; k < s.length(); k++) {
                if (k > 0) {
                    units.append(' ');
                }
                units.append(String.format("%04X", (int) s.charAt(k)));
            }
            emit("charAt*(" + CORPUS_NAMES[i] + ")", units.toString());
        }
        // charAt indexes CODE UNITS: on SUPP index 2 is the HIGH surrogate.
        emit("charAt(SUPP,2)", SUPP.charAt(2));
        emit("charAt(SUPP,3)", SUPP.charAt(3));
        emit("codePointAt(SUPP,2)", SUPP.codePointAt(2));
        expectThrow("charAt(PLAIN,-1)", () -> PLAIN.charAt(-1));
        expectThrow("charAt(PLAIN,len)", () -> PLAIN.charAt(PLAIN.length()));
        expectThrow("charAt(EMPTY,0)", () -> EMPTY.charAt(0));
        expectThrow("charAt(PLAIN,MAX)", () -> PLAIN.charAt(Integer.MAX_VALUE));
        expectThrow("charAt(PLAIN,MIN)", () -> PLAIN.charAt(Integer.MIN_VALUE));
    }

    // ---- isEmpty / substring ---------------------------------------------

    static void isEmptyAndSubstring() {
        for (int i = 0; i < CORPUS.length; i++) {
            String s = CORPUS[i];
            emit("substring1(" + CORPUS_NAMES[i] + ",0)", s.substring(0));
            emit("substring1(" + CORPUS_NAMES[i] + ",len)", s.substring(s.length()));
            if (s.length() >= 2) {
                emit("substring1(" + CORPUS_NAMES[i] + ",1)", s.substring(1));
                emit("substring2(" + CORPUS_NAMES[i] + ",1,2)", s.substring(1, 2));
            }
            emit("substring2(" + CORPUS_NAMES[i] + ",0,len)", s.substring(0, s.length()));
        }
        // Splitting a surrogate PAIR is legal and yields a lone surrogate.
        emit("substring2(SUPP,0,3)", SUPP.substring(0, 3));
        emit("substring1(SUPP,3)", SUPP.substring(3));
        // Identity: substring(0) returns THIS on HotSpot.
        emit("substring1(PLAIN,0)==PLAIN", PLAIN.substring(0) == PLAIN);
        emit("substring2(PLAIN,0,len)==PLAIN", PLAIN.substring(0, PLAIN.length()) == PLAIN);
        expectThrow("substring1(PLAIN,-1)", () -> PLAIN.substring(-1));
        expectThrow("substring1(PLAIN,len+1)", () -> PLAIN.substring(PLAIN.length() + 1));
        expectThrow("substring2(PLAIN,3,2)", () -> PLAIN.substring(3, 2));
        expectThrow("substring2(PLAIN,-1,3)", () -> PLAIN.substring(-1, 3));
        expectThrow("substring2(PLAIN,0,len+1)", () -> PLAIN.substring(0, PLAIN.length() + 1));
        expectThrow("substring2(PLAIN,MIN,MAX)", () -> PLAIN.substring(Integer.MIN_VALUE, Integer.MAX_VALUE));
        // A large-parent substring: the "read only the requested range" native
        // has its own path here.
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < 4096; i++) {
            big.append((char) ('a' + (i % 26)));
        }
        String parent = big.toString();
        emit("substring2(BIG,4000,4005)", parent.substring(4000, 4005));
        emit("substring1(BIG,4090)", parent.substring(4090));
    }

    // ---- startsWith / endsWith / contains / indexOf ------------------------

    static void startsEndsContainsIndexOf() {
        emit("startsWith(PLAIN,\"Hello\")", PLAIN.startsWith("Hello"));
        emit("startsWith(PLAIN,\"\")", PLAIN.startsWith(""));
        emit("startsWith(EMPTY,\"\")", EMPTY.startsWith(""));
        emit("startsWith(EMPTY,\"a\")", EMPTY.startsWith("a"));
        emit("startsWith(PLAIN,PLAIN)", PLAIN.startsWith(PLAIN));
        emit("startsWith(PLAIN,PLAIN+\"x\")", PLAIN.startsWith(PLAIN + "x"));
        emit("startsWith(SUPP,\"ab\")", SUPP.startsWith("ab"));
        // A lone high surrogate as a prefix of a full pair: TRUE by code unit.
        emit("startsWith(SUPP,\"ab\\uD801\")", SUPP.startsWith("ab" + new String(new char[] { '\uD801' })));
        emit("startsWith(PLAIN,\"World\",7)", PLAIN.startsWith("World", 7));
        emit("startsWith(PLAIN,\"World\",-1)", PLAIN.startsWith("World", -1));
        emit("startsWith(PLAIN,\"\",len+5)", PLAIN.startsWith("", PLAIN.length() + 5));
        expectThrow("startsWith(PLAIN,null)", () -> PLAIN.startsWith(null));

        emit("endsWith(PLAIN,\"World\")", PLAIN.endsWith("World"));
        emit("endsWith(PLAIN,\"\")", PLAIN.endsWith(""));
        emit("endsWith(SUPP,\"cd\")", SUPP.endsWith("cd"));
        expectThrow("endsWith(PLAIN,null)", () -> PLAIN.endsWith(null));

        emit("contains(PLAIN,\"lo, W\")", PLAIN.contains("lo, W"));
        emit("contains(PLAIN,\"\")", PLAIN.contains(""));
        emit("contains(SUPP,\"\\uD801\")", SUPP.contains(new String(new char[] { '\uD801' })));
        expectThrow("contains(PLAIN,null)", () -> PLAIN.contains(null));

        emit("indexOf(PLAIN,'o')", PLAIN.indexOf('o'));
        emit("indexOf(PLAIN,'o',5)", PLAIN.indexOf('o', 5));
        emit("indexOf(PLAIN,'z')", PLAIN.indexOf('z'));
        emit("indexOf(PLAIN,'o',-5)", PLAIN.indexOf('o', -5));
        emit("indexOf(PLAIN,'o',999)", PLAIN.indexOf('o', 999));
        // indexOf(int) takes a CODE POINT: the supplementary char is findable.
        emit("indexOf(SUPP,0x10401)", SUPP.indexOf(0x10401));
        // ...and its high surrogate is findable as a code UNIT too.
        emit("indexOf(SUPP,0xD801)", SUPP.indexOf(0xD801));
        emit("indexOf(SUPP,0xDC01)", SUPP.indexOf(0xDC01));
        emit("indexOf(PLAIN,\"o\")", PLAIN.indexOf("o"));
        emit("indexOf(PLAIN,\"\")", PLAIN.indexOf(""));
        emit("indexOf(PLAIN,\"\",5)", PLAIN.indexOf("", 5));
        emit("indexOf(PLAIN,\"\",999)", PLAIN.indexOf("", 999));
        emit("indexOf(REPEAT,\"abc\",1)", REPEAT.indexOf("abc", 1));
        emit("lastIndexOf(PLAIN,'o')", PLAIN.lastIndexOf('o'));
        emit("lastIndexOf(PLAIN,'o',5)", PLAIN.lastIndexOf('o', 5));
        emit("lastIndexOf(REPEAT,\"abc\")", REPEAT.lastIndexOf("abc"));
        emit("lastIndexOf(REPEAT,\"abc\",5)", REPEAT.lastIndexOf("abc", 5));
        emit("lastIndexOf(PLAIN,\"\")", PLAIN.lastIndexOf(""));
        emit("lastIndexOf(SUPP,0x10401)", SUPP.lastIndexOf(0x10401));
        expectThrow("indexOf(PLAIN,(String)null)", () -> PLAIN.indexOf((String) null));
        expectThrow("lastIndexOf(PLAIN,(String)null)", () -> PLAIN.lastIndexOf((String) null));
    }

    // ---- equals / hashCode / compareTo ------------------------------------

    static void equalsHashCodeCompare() {
        for (int i = 0; i < CORPUS.length; i++) {
            emit("hashCode(" + CORPUS_NAMES[i] + ")", CORPUS[i].hashCode());
        }
        // Call twice: the caching native writes the `hash` field on the first
        // call, so a broken cache shows up as a second, different answer.
        for (int i = 0; i < CORPUS.length; i++) {
            emit("hashCode2(" + CORPUS_NAMES[i] + ")", CORPUS[i].hashCode());
        }
        // A string whose hash is 0 must not be re-treated as "uncached".
        String zeroHash = "";
        emit("hashCode(zero-hash-empty)", zeroHash.hashCode());
        emit("hashCode(\"f5a5a608\")", "f5a5a608".hashCode());

        String copy = new String(PLAIN.toCharArray());
        emit("equals(PLAIN,copy)", PLAIN.equals(copy));
        emit("equals(PLAIN,PLAIN)", PLAIN.equals(PLAIN));
        emit("equals(PLAIN,null)", PLAIN.equals(null));
        emit("equals(PLAIN,Integer)", PLAIN.equals(Integer.valueOf(3)));
        emit("equals(PLAIN,StringBuilder)", PLAIN.equals(new StringBuilder(PLAIN)));
        emit("equals(EMPTY,EMPTY2)", EMPTY.equals(new String(new char[0])));
        emit("equals(LONE,LONE-copy)", LONE.equals(new String(LONE.toCharArray())));
        // Latin-1 vs UTF-16 backing for the same content.
        String latin1 = "abc";
        String utf16 = new String(new char[] { 'a', 'b', 'c' });
        emit("equals(latin1,utf16-same-content)", latin1.equals(utf16));
        emit("hashCode-eq(latin1,utf16)", latin1.hashCode() == utf16.hashCode());

        emit("compareTo(\"abc\",\"abd\")", "abc".compareTo("abd"));
        emit("compareTo(\"abc\",\"ab\")", "abc".compareTo("ab"));
        emit("compareTo(\"ab\",\"abc\")", "ab".compareTo("abc"));
        emit("compareTo(EMPTY,EMPTY)", EMPTY.compareTo(""));
        emit("compareTo(SUPP,LONE)", SUPP.compareTo(LONE));
        emit("compareToIgnoreCase(\"ABC\",\"abc\")", "ABC".compareToIgnoreCase("abc"));
        emit("compareToIgnoreCase(TURKISH,TURKISH-lower)", TURKISH.compareToIgnoreCase(TURKISH.toLowerCase(Locale.ROOT)));
        emit("equalsIgnoreCase(\"ABC\",\"abc\")", "ABC".equalsIgnoreCase("abc"));
        emit("equalsIgnoreCase(TURKISH,TURKISH)", TURKISH.equalsIgnoreCase(TURKISH));
        emit("equalsIgnoreCase(PLAIN,null)", PLAIN.equalsIgnoreCase(null));
        expectThrow("compareTo(PLAIN,null)", () -> PLAIN.compareTo(null));
    }

    // ---- case / trim ------------------------------------------------------

    static void caseAndTrim() {
        for (int i = 0; i < CORPUS.length; i++) {
            String s = CORPUS[i];
            emit("toLowerCase-ROOT(" + CORPUS_NAMES[i] + ")", s.toLowerCase(Locale.ROOT));
            emit("toUpperCase-ROOT(" + CORPUS_NAMES[i] + ")", s.toUpperCase(Locale.ROOT));
        }
        Locale tr = Locale.forLanguageTag("tr");
        emit("toLowerCase-tr(TURKISH)", TURKISH.toLowerCase(tr));
        emit("toUpperCase-tr(TURKISH)", TURKISH.toUpperCase(tr));
        emit("toLowerCase-tr(\"TITLE\")", "TITLE".toLowerCase(tr));
        emit("toUpperCase-tr(\"title\")", "title".toUpperCase(tr));
        emit("toLowerCase-ROOT(GREEK)", GREEK.toLowerCase(Locale.ROOT));
        // German sharp s expands on upper-casing: 1 char in, 2 out.
        String sharpS = new String(new char[] { 'a', '\u00DF', 'b' });
        emit("toUpperCase-ROOT(sharp-s)", sharpS.toUpperCase(Locale.ROOT));
        emit("toUpperCase-len(sharp-s)", sharpS.toUpperCase(Locale.ROOT).length());
        // Supplementary-plane casing (Deseret has case).
        emit("toLowerCase-ROOT(SUPP)", SUPP.toLowerCase(Locale.ROOT));
        emit("toLowerCase-len(SUPP)", SUPP.toLowerCase(Locale.ROOT).length());
        // Identity: an already-lowercase ASCII string returns THIS.
        String lower = "already lower";
        emit("toLowerCase-ROOT-identity", lower.toLowerCase(Locale.ROOT) == lower);

        for (int i = 0; i < CORPUS.length; i++) {
            emit("trim(" + CORPUS_NAMES[i] + ")", CORPUS[i].trim());
            emit("strip(" + CORPUS_NAMES[i] + ")", CORPUS[i].strip());
        }
        emit("trim(TRIMMY)-len", TRIMMY.trim().length());
        emit("strip(TRIMMY)-len", TRIMMY.strip().length());
        emit("trim(NBSP)-len", NBSP.trim().length());
        emit("strip(NBSP)-len", NBSP.strip().length());
        String tabs = "\t\na\r\n ";
        emit("trim(tabs)", tabs.trim());
        emit("strip(tabs)", tabs.strip());
        emit("trim-identity(PLAIN)", PLAIN.trim() == PLAIN);
    }

    // ---- concat / replace / split ----------------------------------------

    static void concatReplaceSplit() {
        emit("concat(PLAIN,\"!\")", PLAIN.concat("!"));
        emit("concat(PLAIN,EMPTY)", PLAIN.concat(""));
        emit("concat-identity(PLAIN,EMPTY)", PLAIN.concat("") == PLAIN);
        emit("concat(EMPTY,PLAIN)", EMPTY.concat(PLAIN));
        emit("concat(SUPP,LONE)", SUPP.concat(LONE));
        expectThrow("concat(PLAIN,null)", () -> PLAIN.concat(null));

        emit("replace(PLAIN,'o','0')", PLAIN.replace('o', '0'));
        emit("replace(PLAIN,'z','0')", PLAIN.replace('z', '0'));
        emit("replace-identity(PLAIN,'z','0')", PLAIN.replace('z', '0') == PLAIN);
        emit("replace(SUPP,\\uD801,'?')", SUPP.replace('\uD801', '?'));
        emit("replaceCS(REPEAT,\"abc\",\"X\")", REPEAT.replace("abc", "X"));
        emit("replaceCS(REPEAT,\"\",\"-\")", REPEAT.replace("", "-"));
        emit("replaceCS(EMPTY,\"\",\"-\")", EMPTY.replace("", "-"));
        emit("replaceCS(PLAIN,\"o\",\"$1\")", PLAIN.replace("o", "$1"));
        emit("replaceCS(PLAIN,\"o\",\"\\\\\")", PLAIN.replace("o", "\\"));
        emit("replaceCS(REPEAT,\"aa\",\"X\")", REPEAT.replace("aa", "X"));
        expectThrow("replaceCS(PLAIN,null,\"x\")", () -> PLAIN.replace(null, "x"));

        emit("split(REPEAT,\"b\")", REPEAT.split("b"));
        emit("split(REPEAT,\"b\",2)", REPEAT.split("b", 2));
        emit("split(\"a,b,,\",\",\")", "a,b,,".split(","));
        emit("split(\"a,b,,\",\",\",-1)", "a,b,,".split(",", -1));
        emit("split(\",a\",\",\")", ",a".split(","));
        emit("split(EMPTY,\",\")", EMPTY.split(","));
        emit("split(PLAIN,\"\")", PLAIN.split(""));
        emit("split(PLAIN,\"[o,]\")", PLAIN.split("[o,]"));
        emit("split(PLAIN,\"NOPE\")", PLAIN.split("NOPE"));
        expectThrow("split(PLAIN,\"[\")", () -> PLAIN.split("["));
        expectThrow("split(PLAIN,null)", () -> PLAIN.split(null));
    }

    // ---- the fast-regex family -------------------------------------------

    static void regexFamily() {
        emit("matches(PLAIN,\"H.*d\")", PLAIN.matches("H.*d"));
        emit("matches(PLAIN,\"World\")", PLAIN.matches("World"));
        emit("matches(EMPTY,\"\")", EMPTY.matches(""));
        emit("matches(SUPP,\"ab.*cd\")", SUPP.matches("ab.*cd"));
        emit("matches(PLAIN,\"^Hello.*$\")", PLAIN.matches("^Hello.*$"));
        expectThrow("matches(PLAIN,\"[\")", () -> PLAIN.matches("["));
        expectThrow("matches(PLAIN,null)", () -> PLAIN.matches(null));

        emit("replaceAll(REPEAT,\"a(b)c\",\"<$1>\")", REPEAT.replaceAll("a(b)c", "<$1>"));
        emit("replaceAll(REPEAT,\"b\",\"\")", REPEAT.replaceAll("b", ""));
        emit("replaceAll(PLAIN,\"o\",\"0\")", PLAIN.replaceAll("o", "0"));
        emit("replaceAll(PLAIN,\"\",\"-\")", PLAIN.replaceAll("", "-"));
        emit("replaceAll(PLAIN,\"l+\",\"L\")", PLAIN.replaceAll("l+", "L"));
        emit("replaceAll(PLAIN,\"(l)\",\"[$1$1]\")", PLAIN.replaceAll("(l)", "[$1$1]"));
        emit("replaceAll-backslash", "a.b".replaceAll("\\.", "\\\\"));
        emit("replaceAll-named", "john smith".replaceAll("(?<first>\\w+) (?<last>\\w+)", "${last}, ${first}"));
        emit("replaceAll-dollar-literal", "x".replaceAll("x", java.util.regex.Matcher.quoteReplacement("$0")));
        expectThrow("replaceAll-bad-group", () -> "abc".replaceAll("(a)", "$9"));
        expectThrow("replaceAll(PLAIN,\"[\",\"x\")", () -> PLAIN.replaceAll("[", "x"));

        emit("replaceFirst(REPEAT,\"abc\",\"X\")", REPEAT.replaceFirst("abc", "X"));
        emit("replaceFirst(REPEAT,\"z\",\"X\")", REPEAT.replaceFirst("z", "X"));
        emit("replaceFirst(PLAIN,\"\",\"-\")", PLAIN.replaceFirst("", "-"));
        emit("replaceFirst(PLAIN,\"(l)(l)\",\"$2$1\")", PLAIN.replaceFirst("(l)(l)", "$2$1"));
        expectThrow("replaceFirst(PLAIN,null,\"x\")", () -> PLAIN.replaceFirst(null, "x"));
    }

    // ---- the charset-name constructors -----------------------------------

    static void charsetConstructors() throws Exception {
        byte[] utf8 = PLAIN.getBytes(StandardCharsets.UTF_8);
        byte[] suppUtf8 = SUPP.getBytes(StandardCharsets.UTF_8);
        byte[] latin1 = new byte[] { (byte) 0xe9, 'a', (byte) 0xff };

        emit("new String(utf8,\"UTF-8\")", new String(utf8, "UTF-8"));
        emit("new String(utf8,\"utf-8\")", new String(utf8, "utf-8"));
        emit("new String(utf8,\"UTF8\")", new String(utf8, "UTF8"));
        emit("new String(suppUtf8,\"UTF-8\")", new String(suppUtf8, "UTF-8"));
        emit("new String(suppUtf8,\"UTF-8\").length", new String(suppUtf8, "UTF-8").length());
        emit("new String(latin1,\"ISO-8859-1\")", new String(latin1, "ISO-8859-1"));
        emit("new String(latin1,\"US-ASCII\")", new String(latin1, "US-ASCII"));
        emit("new String(utf8,1,4,\"UTF-8\")", new String(utf8, 1, 4, "UTF-8"));
        emit("new String(utf8,0,0,\"UTF-8\")", new String(utf8, 0, 0, "UTF-8"));
        // A truncated multi-byte sequence must become U+FFFD, not throw.
        byte[] truncated = new byte[] { (byte) 0xe4, (byte) 0xb8 };
        emit("new String(truncated,\"UTF-8\")", new String(truncated, "UTF-8"));
        emit("roundtrip(SUPP,UTF-8)", new String(SUPP.getBytes("UTF-8"), "UTF-8").equals(SUPP));
        // A LONE surrogate does NOT round-trip: it encodes to '?' (0x3f).
        emit("getBytes(LONE,UTF-8)", LONE.getBytes("UTF-8"));
        emit("roundtrip(LONE,UTF-8)", new String(LONE.getBytes("UTF-8"), "UTF-8").equals(LONE));

        expectThrow("new String(utf8,\"NO-SUCH\")", () -> new String(utf8, "NO-SUCH"));
        expectThrow("new String(utf8,(String)null)", () -> new String(utf8, (String) null));
        expectThrow("new String(null,\"UTF-8\")", () -> new String((byte[]) null, "UTF-8"));
        expectThrow("new String(utf8,-1,2,\"UTF-8\")", () -> new String(utf8, -1, 2, "UTF-8"));
        expectThrow("new String(utf8,0,999,\"UTF-8\")", () -> new String(utf8, 0, 999, "UTF-8"));
    }

    // ---- paired properties ------------------------------------------------
    // Each of these ties two shapes together, so a native that is wrong in a
    // way a single-value check cannot see still fails.

    static void pairedProperties() {
        for (int i = 0; i < CORPUS.length; i++) {
            String s = CORPUS[i];
            String tag = CORPUS_NAMES[i];
            // substring(0,k) + substring(k) reconstitutes the whole string, for
            // every k, INCLUDING one that splits a surrogate pair.
            boolean rebuilt = true;
            for (int k = 0; k <= s.length(); k++) {
                if (!(s.substring(0, k) + s.substring(k)).equals(s)) {
                    rebuilt = false;
                }
            }
            emit("PAIR substring-splits-rejoin(" + tag + ")", rebuilt);
            // length() agrees with the number of charAt calls that succeed.
            int counted = 0;
            try {
                while (true) {
                    s.charAt(counted);
                    counted++;
                }
            } catch (RuntimeException expected) {
                // fallthrough
            }
            emit("PAIR charAt-count==length(" + tag + ")", counted == s.length());
            // isEmpty() agrees with length()==0.
            emit("PAIR isEmpty==len0(" + tag + ")", s.isEmpty() == (s.length() == 0));
            // startsWith(prefix) is true for every prefix substring(0,k).
            boolean allPrefixes = true;
            for (int k = 0; k <= s.length(); k++) {
                if (!s.startsWith(s.substring(0, k))) {
                    allPrefixes = false;
                }
            }
            emit("PAIR startsWith-every-prefix(" + tag + ")", allPrefixes);
            // equals is reflexive through a char[] round trip, and hashCode
            // agrees with it.
            String copy = new String(s.toCharArray());
            emit("PAIR equals-copy&&hash(" + tag + ")",
                    s.equals(copy) && s.hashCode() == copy.hashCode());
            // compareTo==0 exactly when equals.
            emit("PAIR compareTo0==equals(" + tag + ")",
                    (s.compareTo(copy) == 0) == s.equals(copy));
            // indexOf/lastIndexOf of the whole string is 0 / 0.
            emit("PAIR indexOf-self(" + tag + ")",
                    s.indexOf(s) == 0 && s.lastIndexOf(s) == 0);
            // endsWith(suffix) for every suffix.
            boolean allSuffixes = true;
            for (int k = 0; k <= s.length(); k++) {
                if (!s.endsWith(s.substring(k))) {
                    allSuffixes = false;
                }
            }
            emit("PAIR endsWith-every-suffix(" + tag + ")", allSuffixes);
            // contains(sub) for every substring.
            boolean allContained = true;
            for (int a = 0; a <= s.length(); a++) {
                for (int b = a; b <= s.length(); b++) {
                    if (!s.contains(s.substring(a, b))) {
                        allContained = false;
                    }
                }
            }
            emit("PAIR contains-every-substring(" + tag + ")", allContained);
            // trim() is idempotent and is a substring of the original.
            emit("PAIR trim-idempotent(" + tag + ")",
                    s.trim().trim().equals(s.trim()) && s.contains(s.trim()));
            // concat then substring back.
            emit("PAIR concat-then-split(" + tag + ")",
                    s.concat(PLAIN).substring(0, s.length()).equals(s));
        }
    }
}
