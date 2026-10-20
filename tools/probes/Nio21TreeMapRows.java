import java.lang.reflect.InvocationTargetException;
import java.net.http.HttpHeaders;
import java.util.*;
import java.util.function.BiPredicate;

/**
 * nio21 wave 5 lane L4: conformance and bench rows for the `--jdk-only` `TreeMap` /
 * `HttpHeaders` JIT path (docs/internal/fixed-bugs/r20-jdk-http-client-throughput-is-ten-to-fifty-x-hotspot-CLOSED-20261012.md).
 *
 * Two JIT changes are under test:
 *
 *  * `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY` (default ON): the compiled caller splices the JDK's
 *    own body of `StringLatin1.getChar([BI)C`, `Boolean.booleanValue()`, `Character.charValue()`
 *    instead of calling the registered `Intrinsic` native (a native-funnel crossing per character
 *    of every `String.CASE_INSENSITIVE_ORDER` compare). Since 2026-10-13 `getChar` has no native
 *    at all and is plain bytecode.
 *  * `CRATONVM_JIT_IR_SPLICE_FORWARDER_EXACT_HOT` (default ON): the guarded splice of a javac
 *    BRIDGE (`CaseInsensitiveComparator.compare(Object,Object)`) may take its target
 *    (`compare(String,String)`, 68 bytes) under the hot cap instead of the cold 35-byte one.
 *
 * Conformance (default): every line is a fact computed after a warm loop long enough for the
 * compiled code to answer (comparison results, map contents and orders, header answers, the
 * exception types and messages of the null and out-of-range shapes). Diff against HotSpot 25.
 *
 *   javac -d out tools/probes/Nio21TreeMapRows.java
 *   java -cp out Nio21TreeMapRows > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21TreeMapRows > jo.txt
 *   cratonvm --compatible -cp out Nio21TreeMapRows > co.txt
 *   CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY=0 CRATONVM_JIT_IR_SPLICE_FORWARDER_EXACT_HOT=0 \
 *       cratonvm --jdk-only -cp out Nio21TreeMapRows > jo0.txt
 *
 * hs, jo, co and jo0 must be identical. -Dnio21.rows=a,b selects conformance rows
 * (ciflip, rmflip, ci, ignorecase, tmapci, tmapnat, tmapcmp, poly, hdr, box, bigcmp).
 *
 * nio21 wave 6 lane L4 adds three switches and four rows:
 *
 *  * `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP` (default ON): a profile-hot one-class guarded site
 *    takes a body up to 325 bytes instead of 35 -- `bigcmp` (a `Comparator<Object>` and a
 *    `BiPredicate` whose real bodies are over 35 bytes, then a second class on the same site:
 *    the miss edge) and the bench row `tmapbig`.
 *  * `CRATONVM_JIT_IR_SPLICE_NEVER_RUN_STATIC_EXIT` (default ON): a static call whose callee never
 *    ran is a precise exit, not a reason to refuse the whole spliced body --
 *    `CaseInsensitiveComparator.compare(String,String)`'s and `regionMatches`' UTF-16 arms.
 *    `ciflip` / `rmflip` warm on Latin-1 only, THEN hand the same compiled sites UTF-16 strings
 *    (the exits fire), then warm again. Run them alone too (`-Dnio21.rows=ciflip` and
 *    `-Dnio21.rows=rmflip`): in the `all` run they are first, so nothing else ran UTF-16 before.
 *  * `CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_CALLS` (default ON): `String.regionMatches(ZILString;II)Z`
 *    splices its JDK body in place of the `Intrinsic` native -- `rmflip`, `ignorecase`, and the
 *    bench row `eqic`.
 *
 * nio21 wave 8 lane L5 adds the row `cpci` (`-Dnio21.rows=cpci`): `toLowerCase` code points,
 * `equalsIgnoreCase` both orders, `compareToIgnoreCase` values, and `regionMatches(true, ..)` at
 * offsets that keep or split a surrogate pair, for Deseret / Adlam / Old Hungarian / Medefaidrin
 * capitals and for unpaired surrogates (switch `CRATONVM_STRING_CI_CODE_POINT`, default ON).
 *
 * Bench (`<row> <N> ns/op`): -Dnio21.bench=1, rows tmap tmapnat cicmp hdr tmapcmp box tmapbig eqic
 * (-Dnio21.rows=tmap,hdr selects; -Dnio21.iters=N default 100000, -Dnio21.warm=N default 3 warm
 * passes, then 5 timed, the median printed). `tmap` and `hdr` are `Nio20HttpHot`'s rows of the same
 * name. Interleave HotSpot, `--jdk-only` and `--jdk-only` with both switches `=0`.
 */
public class Nio21TreeMapRows {
    static final String ROWS = System.getProperty("nio21.rows", "all");
    static final int WARM = 20_000;

    static boolean on(String r) {
        return ROWS.equals("all") || Arrays.asList(ROWS.split(",")).contains(r);
    }

    /** Print one line, every non-ASCII char as a backslash-u escape (stdout encodings differ). */
    static void say(String s) {
        StringBuilder sb = new StringBuilder(s.length() + 16);
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x80) sb.append(c);
            else sb.append(String.format("\\u%04x", (int) c));
        }
        System.out.println(sb);
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    public static void main(String[] a) throws Throwable {
        Locale.setDefault(Locale.US);
        String b = System.getProperty("nio21.bench", "");
        if (b.equals("1") || b.equals("true") || (a.length > 0 && a[0].equals("bench"))) {
            bench();
            return;
        }
        String[] all = {"ciflip", "rmflip", "ci", "ignorecase", "tmapci", "tmapnat", "tmapcmp", "poly",
            "hdr", "box", "bigcmp", "cpci"};
        for (String r : all) {
            if (!on(r)) continue;
            try {
                Nio21TreeMapRows.class.getDeclaredMethod(r).invoke(null);
            } catch (InvocationTargetException e) {
                say(r + " THREW " + ex(e.getCause()));
            }
        }
        say("done");
    }

    // ---- shared data ------------------------------------------------------------------------

    /** Latin-1 and UTF-16 strings, case variants, prefixes, the special folds. */
    static final String[] WORDS = {
        "", "a", "A", "b", "Date", "date", "DATE", "Content-Length", "content-length",
        "CONTENT-LENGTH", "Content-Type", "Connection", "x-probe", "X-Probe", "Host", "host",
        "abc", "abd", "ab", "ABCD", "\u00e9t\u00e9", "\u00c9T\u00c9", "\u00ff", "\u0178",
        "\u00b5", "\u039c", "stra\u00dfe", "STRASSE", "\u03a3\u03b1", "\u03c3\u0391",
        "\u01c5", "\u01c4", "\u01c6", "a\u0130", "A\u0131", "k\u212a", "K\u212a", "\ud801\udc00",
        "\ud801\udc28", "zz", "Zz", "@", "`", "[", "{"
    };

    static int sign(int v) {
        return Integer.signum(v);
    }

    // ---- conformance rows --------------------------------------------------------------------

    /** `String.CASE_INSENSITIVE_ORDER.compare` over every pair, exact values, after a warm loop. */
    static void ci() {
        Comparator<String> c = String.CASE_INSENSITIVE_ORDER;
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            String x = WORDS[i % WORDS.length];
            String y = WORDS[(i * 7 + 3) % WORDS.length];
            sum += c.compare(x, y);
        }
        say("ci warm " + sum);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < WORDS.length; i++) {
            sb.setLength(0);
            for (int j = 0; j < WORDS.length; j++) {
                sb.append(c.compare(WORDS[i], WORDS[j])).append(' ');
            }
            say("ci " + i + ": " + sb.toString().trim());
        }
    }

    /** `compareToIgnoreCase`, `equalsIgnoreCase`, `regionMatches(true, ..)`, hot. */
    static void ignorecase() {
        long sum = 0;
        for (int i = 0; i < WARM; i++) {
            String x = WORDS[i % WORDS.length];
            String y = WORDS[(i * 5 + 1) % WORDS.length];
            sum += x.compareToIgnoreCase(y) + (x.equalsIgnoreCase(y) ? 1000 : 0)
                    + (x.regionMatches(true, 0, y, 0, Math.min(x.length(), y.length())) ? 7 : 0);
        }
        say("ignorecase warm " + sum);
        for (int i = 0; i < WORDS.length; i += 3) {
            String x = WORDS[i];
            String y = WORDS[(i + 1) % WORDS.length];
            say("ignorecase " + i + " " + x.compareToIgnoreCase(y) + " "
                    + x.equalsIgnoreCase(y) + " " + x.toUpperCase().equalsIgnoreCase(x) + " "
                    + x.regionMatches(true, 0, y, 0, Math.min(x.length(), y.length())));
        }
        try {
            say("ignorecase null " + "a".compareToIgnoreCase(null));
        } catch (Throwable t) {
            say("ignorecase null " + t.getClass().getName());
        }
    }

    static final String[] NAMES = {"Date", "Content-length", "Content-Type", "X-probe", "Connection", "Host"};
    static final String[] ASKS = {"date", "CONTENT-LENGTH", "content-type", "x-Probe", "connection", "HOST",
        "missing", "", "Hos", "Hostt"};

    /** The `tmap` shape: a case-insensitive `TreeMap`, puts, gets, iteration, replace. */
    static void tmapci() {
        long sum = 0;
        TreeMap<String, String> last = null;
        for (int i = 0; i < WARM; i++) {
            TreeMap<String, String> m = new TreeMap<>(String.CASE_INSENSITIVE_ORDER);
            for (String s : NAMES) m.put(s, s);
            for (String s : ASKS) {
                String v = m.get(s);
                sum += v == null ? -1 : v.length();
            }
            for (Map.Entry<String, String> e : m.entrySet()) sum += e.getKey().charAt(0);
            last = m;
        }
        say("tmapci warm " + sum);
        say("tmapci map " + last);
        say("tmapci put-old " + last.put("HOST", "h2") + " now " + last.get("host")
                + " key " + last.ceilingKey("HOST") + " size " + last.size());
        say("tmapci first " + last.firstKey() + " last " + last.lastKey()
                + " floor " + last.floorKey("D") + " higher " + last.higherKey("content-length")
                + " head " + last.headMap("D") + " tail " + last.tailMap("x"));
        say("tmapci remove " + last.remove("X-PROBE") + " " + last.containsKey("x-probe")
                + " " + last.descendingMap());
        try {
            last.put(null, "n");
            say("tmapci null put ok");
        } catch (Throwable t) {
            say("tmapci null put " + t.getClass().getName());
        }
    }

    /** Natural `String` order (the `Comparable.compareTo` branch), and `Integer` keys. */
    static void tmapnat() {
        long sum = 0;
        TreeMap<String, Integer> s = null;
        TreeMap<Integer, Integer> n = null;
        for (int i = 0; i < WARM; i++) {
            s = new TreeMap<>();
            for (int k = 0; k < WORDS.length; k += 2) s.put(WORDS[k], k);
            sum += s.getOrDefault(WORDS[i % WORDS.length], -1);
            n = new TreeMap<>();
            for (int k = 0; k < 8; k++) n.put((k * 37 + i) % 11, k);
            sum += n.firstKey() + n.lastKey() + n.size();
        }
        say("tmapnat warm " + sum);
        say("tmapnat strings " + s);
        say("tmapnat ints " + n);
        try {
            TreeMap<Object, Object> mixed = new TreeMap<>();
            mixed.put("a", 1);
            mixed.put(1, 2);
            say("tmapnat mixed ok");
        } catch (Throwable t) {
            say("tmapnat mixed " + t.getClass().getName());
        }
    }

    /** A comparator class (with its javac bridge) and a lambda, each on its own map. */
    static final class ByLength implements Comparator<String> {
        @Override
        public int compare(String x, String y) {
            int d = x.length() - y.length();
            return d != 0 ? d : x.compareTo(y);
        }
    }

    static void tmapcmp() {
        long sum = 0;
        TreeMap<String, Integer> byLen = null;
        TreeMap<String, Integer> rev = null;
        Comparator<String> lam = (x, y) -> y.compareTo(x);
        for (int i = 0; i < WARM; i++) {
            byLen = new TreeMap<>(new ByLength());
            rev = new TreeMap<>(lam);
            for (int k = 0; k < WORDS.length; k += 3) {
                byLen.put(WORDS[k], k);
                rev.put(WORDS[k], k);
            }
            sum += byLen.getOrDefault(WORDS[i % WORDS.length], -1) + rev.getOrDefault(WORDS[i % 7], -2);
        }
        say("tmapcmp warm " + sum);
        say("tmapcmp bylen " + byLen);
        say("tmapcmp rev " + rev);
    }

    /** One `TreeMap.put` site fed three comparators in turn: a polymorphic profile. */
    static void poly() {
        List<Comparator<String>> cs = List.of(String.CASE_INSENSITIVE_ORDER, new ByLength(),
                Comparator.<String>reverseOrder());
        long sum = 0;
        List<TreeMap<String, Integer>> lastMaps = new ArrayList<>();
        for (int i = 0; i < WARM; i++) {
            TreeMap<String, Integer> m = new TreeMap<>(cs.get(i % 3));
            for (int k = 0; k < 12; k++) m.put(WORDS[(k * 5 + i) % WORDS.length], k);
            sum += m.size() + m.firstKey().length();
            if (i >= WARM - 3) lastMaps.add(m);
        }
        say("poly warm " + sum);
        for (TreeMap<String, Integer> m : lastMaps) say("poly map " + m);
    }

    static final Map<String, List<String>> RAW = new LinkedHashMap<>();

    static {
        RAW.put("Date", List.of("Thu, 08 Oct 2026 10:00:00 GMT"));
        RAW.put("Content-length", List.of("1024"));
        RAW.put("Content-Type", List.of("application/octet-stream"));
        RAW.put("X-probe", List.of("nio20"));
        RAW.put("Connection", List.of("keep-alive"));
        RAW.put("Set-Cookie", List.of("a=1", "b=2"));
    }

    /** `HttpHeaders.of` and its readers, as `Nio20HttpHot hdr` runs them. */
    static void hdr() {
        long sum = 0;
        BiPredicate<String, String> all = (k, v) -> true;
        BiPredicate<String, String> noCookie = (k, v) -> !k.equalsIgnoreCase("set-cookie");
        HttpHeaders h = null;
        for (int i = 0; i < WARM / 4; i++) {
            h = HttpHeaders.of(RAW, (i & 1) == 0 ? all : noCookie);
            sum += h.firstValueAsLong("content-length").orElse(-1);
            sum += h.firstValue("CONNECTION").map(String::length).orElse(-1);
            sum += h.allValues("set-cookie").size();
            sum += h.firstValue("transfer-encoding").isPresent() ? 7 : 3;
            for (Map.Entry<String, List<String>> e : h.map().entrySet()) {
                sum += e.getKey().length() + e.getValue().size();
            }
        }
        say("hdr warm " + sum);
        h = HttpHeaders.of(RAW, all);
        say("hdr map " + h.map());
        say("hdr first " + h.firstValue("date") + " " + h.firstValue("DATE")
                + " long " + h.firstValueAsLong("Content-Length") + " all " + h.allValues("SET-COOKIE")
                + " none " + h.allValues("nope") + " eq " + h.equals(HttpHeaders.of(RAW, all))
                + " hash " + (h.hashCode() == HttpHeaders.of(RAW, all).hashCode()));
        HttpHeaders f = HttpHeaders.of(RAW, noCookie);
        say("hdr filtered " + f.map().keySet());
        try {
            HttpHeaders.of(RAW, all).firstValueAsLong("date");
            say("hdr nfe none");
        } catch (Throwable t) {
            say("hdr nfe " + ex(t));
        }
        try {
            Map<String, List<String>> dup = new LinkedHashMap<>();
            dup.put("Accept", List.of("a"));
            dup.put("ACCEPT", List.of("b"));
            say("hdr dup " + HttpHeaders.of(dup, all).map());
        } catch (Throwable t) {
            say("hdr dup " + ex(t));
        }
    }

    /** `Boolean.booleanValue` / `Character.charValue` unboxing, hot, and the null receiver. */
    static void box() {
        Boolean[] bs = {Boolean.TRUE, Boolean.FALSE, Boolean.valueOf("TRUE"), Boolean.valueOf(false)};
        Character[] cs = {'a', 'Z', '\u00e9', '\u4e2d', Character.valueOf((char) 0)};
        long sum = 0;
        for (int i = 0; i < WARM * 5; i++) {
            Boolean b = bs[i & 3];
            Character c = cs[i % 5];
            if (b.booleanValue()) sum += c.charValue();
            else sum -= 1;
        }
        say("box warm " + sum);
        Boolean nb = null;
        Character nc = null;
        for (int i = 0; i < 3; i++) {
            try {
                say("box null bool " + unboxBool(i == 2 ? nb : bs[i]));
            } catch (Throwable t) {
                say("box null bool " + t.getClass().getName());
            }
            try {
                say("box null char " + (int) unboxChar(i == 2 ? nc : cs[i]));
            } catch (Throwable t) {
                say("box null char " + t.getClass().getName());
            }
        }
    }

    // ---- nio21 wave 8 L5 rows -----------------------------------------------------------------

    /** Supplementary capitals: Deseret, Adlam, Old Hungarian, Medefaidrin (all have a 1:1 lowercase). */
    static final int[] SUPP = {0x10400, 0x1E900, 0x10C80, 0x16E40, 0x10401};

    static String cps(String s) {
        StringBuilder sb = new StringBuilder();
        s.codePoints().forEach(c -> sb.append(Integer.toHexString(c)).append('.'));
        return sb.toString();
    }

    static int cmpAt(String x, String y) {
        return x.compareToIgnoreCase(y);
    }

    static String tf(boolean b) {
        return b ? "T" : "f";
    }

    /** One pass of every `cpci` fact; `out` null = warm-up only (the answers still feed `h`). */
    static long cpciPass(List<String> out) {
        long h = 0;
        for (int cp : SUPP) {
            String u = new String(Character.toChars(cp));
            String l = u.toLowerCase();
            String u2 = new String(l.toUpperCase());
            String lc = new String(l);
            StringBuilder sb = new StringBuilder();
            sb.append(cps(l)).append(' ').append(cps(u2)).append(' ')
                    .append(Integer.toHexString(Character.toLowerCase(cp))).append(' ')
                    .append(Integer.toHexString(Character.toUpperCase(Character.toLowerCase(cp)))).append(' ');
            sb.append(tf(eqAt(l, u))).append(tf(eqAt(u, l))).append(tf(eqAt(lc, l))).append(tf(eqAt(u2, u)))
                    .append(tf(eqAt("x" + u + "Y", "X" + l + "y"))).append(tf(eqAt(u, l.substring(0, 1) + "a")))
                    .append(' ');
            sb.append(cmpAt(l, u)).append(',').append(cmpAt(u, l)).append(',')
                    .append(cmpAt(u, new String(Character.toChars(Character.toLowerCase(cp) + 1)))).append(',')
                    .append(cmpAt("a", u)).append(',').append(cmpAt(u, "a")).append(',')
                    .append(cmpAt("Ā" + u, "ā" + l + "z")).append(' ');
            // regionMatches(true, ..) at offsets that keep or split the pair.
            String xu = "x" + u;
            String yl = "Y" + l;
            sb.append(tf(rmAt(u, 0, l, 0, 2))).append(tf(rmAt(u, 0, l, 0, 1))).append(tf(rmAt(u, 1, l, 1, 1)))
                    .append(tf(rmAt(xu, 1, l, 0, 2))).append(tf(rmAt(l, 0, xu, 1, 2))).append(tf(rmAt(xu, 0, yl, 0, 3)))
                    .append(tf(rmAt(xu, 2, yl, 2, 1))).append(tf(rmAt(xu, 0, yl, 0, 2)));
            String line = "cpci " + Integer.toHexString(cp) + ": " + sb;
            h = h * 31 + line.hashCode();
            if (out != null) out.add(line);
        }
        // Unpaired surrogates, and the JDK's index skew (forward pair vs a back-paired low half).
        String[][] pairs = {
            {"\ud801", "\ud801"}, {"\udc00", "\udc28"}, {"\ud801a", "\ud801A"}, {"\udc00\ud801", "\udc28\ud801"},
            {"\ud801𐐀", "𐐨A"}, {"𐐨A", "\ud801𐐀"},
            {"q\udc00", "Q\udc28"}, {"\ud801x", "𐐨"}, {"𞤢\ud801", "𞤀\ud801"}
        };
        for (String[] p : pairs) {
            String line = "cpci unpaired " + cps(p[0]) + " " + cps(p[1]) + ": " + tf(eqAt(p[0], p[1]))
                    + tf(eqAt(p[1], p[0])) + " " + cmpAt(p[0], p[1]) + "," + cmpAt(p[1], p[0]) + " "
                    + tf(rmAt(p[0], 0, p[1], 0, Math.min(p[0].length(), p[1].length())))
                    + tf(rmAt(p[0], 1, p[1], 1, Math.min(p[0].length(), p[1].length()) - 1));
            h = h * 31 + line.hashCode();
            if (out != null) out.add(line);
        }
        return h;
    }

    /**
     * nio21 wave 8 L5: `equalsIgnoreCase` / `compareToIgnoreCase` / `regionMatches(true, ..)` of
     * supplementary letters. HotSpot's `StringUTF16.compareToCIImpl` folds a surrogate PAIR as one
     * code point; the VM's natives folded code units and answered `"𐐨".equalsIgnoreCase("𐐀")`
     * `false`. Cold answers first, then the same answers after a warm loop.
     */
    static void cpci() {
        List<String> cold = new ArrayList<>();
        cpciPass(cold);
        for (String s : cold) say(s);
        long h = 0;
        for (int i = 0; i < WARM / 10; i++) h += cpciPass(null);
        List<String> warm = new ArrayList<>();
        cpciPass(warm);
        say("cpci warm-same " + warm.equals(cold) + " " + (h == (WARM / 10) * cpciPass(null)));
        if (!warm.equals(cold)) for (String s : warm) say("warm " + s);
    }

    // ---- nio21 wave 6 L4 rows -----------------------------------------------------------------

    static final String[] LATIN = {"Date", "date", "Content-Length", "content-type", "Host", "x-probe",
        "abc", "ABD", "Connection", "CONNECTION", "", "h"};
    static final String[] WIDE = {"été", "Σα", "Straße", "𐐀", "İx",
        "date‐", "DATE‐", "HostK", "hostK"};

    /** One compiled `Comparator.compare` site, fed by `ciflip`. */
    static int ciAt(Comparator<String> c, String x, String y) {
        return c.compare(x, y);
    }

    /** `CASE_INSENSITIVE_ORDER` warmed on Latin-1 only, then given UTF-16 strings at the same site. */
    static void ciflip() {
        Comparator<String> c = String.CASE_INSENSITIVE_ORDER;
        long sum = 0;
        for (int i = 0; i < WARM * 2; i++) {
            sum += ciAt(c, LATIN[i % LATIN.length], LATIN[(i * 5 + 1) % LATIN.length]);
        }
        say("ciflip warm " + sum);
        StringBuilder sb = new StringBuilder();
        for (String w : WIDE) {
            sb.setLength(0);
            for (String l : LATIN) {
                sb.append(ciAt(c, w, l)).append(',').append(ciAt(c, l, w)).append(' ');
            }
            sb.append(ciAt(c, w, w.toUpperCase())).append(' ').append(ciAt(c, w.toLowerCase(), w));
            say("ciflip " + w + ": " + sb.toString().trim());
        }
        TreeMap<String, Integer> m = new TreeMap<>(c);
        for (int i = 0; i < LATIN.length; i++) m.put(LATIN[i], i);
        for (int i = 0; i < WIDE.length; i++) m.put(WIDE[i], 100 + i);
        say("ciflip map " + m);
        sum = 0;
        for (int i = 0; i < WARM; i++) {
            String w = WIDE[i % WIDE.length];
            sum += ciAt(c, w, LATIN[i % LATIN.length]) + ciAt(c, LATIN[(i * 3) % LATIN.length], w);
        }
        say("ciflip rewarm " + sum);
    }

    /** One compiled `equalsIgnoreCase` and one `regionMatches(true, ..)` site, fed by `rmflip`. */
    static boolean eqAt(String x, String y) {
        return x.equalsIgnoreCase(y);
    }

    static boolean rmAt(String x, int xo, String y, int yo, int n) {
        return x.regionMatches(true, xo, y, yo, n);
    }

    /** `equalsIgnoreCase` / `regionMatches(true, ..)` warmed on Latin-1, then UTF-16, then bad ranges. */
    static void rmflip() {
        long sum = 0;
        for (int i = 0; i < WARM * 2; i++) {
            String x = LATIN[i % LATIN.length];
            String y = LATIN[(i * 7 + 1) % LATIN.length];
            sum += (eqAt(x, y) ? 3 : 1) + (rmAt(x, 0, y, 0, Math.min(x.length(), y.length())) ? 5 : 0);
        }
        say("rmflip warm " + sum);
        for (String w : WIDE) {
            StringBuilder sb = new StringBuilder();
            for (String l : LATIN) {
                sb.append(eqAt(w, l) ? 'T' : 'f').append(eqAt(l, w) ? 'T' : 'f')
                        .append(rmAt(w, 0, l, 0, Math.min(w.length(), l.length())) ? 'T' : 'f')
                        .append(rmAt(l, 0, w, 0, Math.min(w.length(), l.length())) ? 'T' : 'f').append(' ');
            }
            sb.append(eqAt(w, w.toUpperCase())).append(' ').append(eqAt(w.toLowerCase(), w)).append(' ')
                    .append(rmAt(w, 1, w.toUpperCase(), 1, w.length() - 1));
            say("rmflip " + w + ": " + sb.toString().trim());
        }
        say("rmflip ranges " + rmAt("Host", -1, "host", 0, 2) + " " + rmAt("Host", 0, "host", -1, 2)
                + " " + rmAt("Host", 3, "host", 3, 2) + " " + rmAt("Host", 0, "host", 0, -3)
                + " " + rmAt("Host", 4, "host", 4, 0) + " " + rmAt("été", 2, "ÉTÉ", 2, 1));
        try {
            say("rmflip null " + rmAt("Host", 0, null, 0, 1));
        } catch (Throwable t) {
            say("rmflip null " + ex(t));
        }
        say("rmflip eqnull " + eqAt("Host", null));
        sum = 0;
        for (int i = 0; i < WARM; i++) {
            String w = WIDE[i % WIDE.length];
            String l = LATIN[i % LATIN.length];
            sum += (eqAt(w, w.toUpperCase()) ? 3 : 1) + (rmAt(l, 0, w, 0, Math.min(l.length(), w.length())) ? 5 : 0);
        }
        say("rmflip rewarm " + sum);
    }

    /** A comparator whose `compare(Object,Object)` is its real body (no bridge), over 35 bytes. */
    static final class CharOrder implements Comparator<Object> {
        @Override
        public int compare(Object a, Object b) {
            String x = (String) a;
            String y = (String) b;
            int lx = x.length();
            int ly = y.length();
            int n = lx < ly ? lx : ly;
            for (int i = 0; i < n; i++) {
                char cx = x.charAt(i);
                char cy = y.charAt(i);
                if (cx != cy) {
                    return cx - cy;
                }
            }
            return lx - ly;
        }
    }

    /** A named `BiPredicate` whose `test` is over 35 bytes. */
    static final class LongHeader implements BiPredicate<String, String> {
        @Override
        public boolean test(String k, String v) {
            int n = 0;
            for (int i = 0; i < k.length(); i++) {
                if (k.charAt(i) == '-') n++;
            }
            return n > 0 && v.length() > n && k.length() + v.length() > 12;
        }
    }

    /** One compiled `Comparator.compare` site and one `BiPredicate.test` site, fed by `bigcmp`. */
    static int cmpAt(Comparator<Object> c, Object x, Object y) {
        return c.compare(x, y);
    }

    static boolean testAt(BiPredicate<String, String> p, String k, String v) {
        return p.test(k, v);
    }

    /** The hot guarded splice of a body over 35 bytes, then another class on the miss edge. */
    static void bigcmp() {
        Comparator<Object> c = new CharOrder();
        BiPredicate<String, String> p = new LongHeader();
        long sum = 0;
        for (int i = 0; i < WARM * 2; i++) {
            String x = WORDS[i % WORDS.length];
            String y = WORDS[(i * 7 + 3) % WORDS.length];
            sum += cmpAt(c, x, y) + (testAt(p, x, y) ? 11 : 0);
        }
        say("bigcmp warm " + sum);
        TreeMap<String, Integer> m = new TreeMap<>(c);
        for (int k = 0; k < WORDS.length; k += 2) m.put(WORDS[k], k);
        say("bigcmp map " + m);
        say("bigcmp hit " + cmpAt(c, "Content-Length", "Content-Type") + " " + cmpAt(c, "ab", "abc")
                + " " + testAt(p, "Content-Length", "1024") + " " + testAt(p, "Host", "x"));
        Comparator<Object> other = (x, y) -> ((String) y).length() - ((String) x).length();
        BiPredicate<String, String> otherP = (k, v) -> k.isEmpty();
        say("bigcmp miss " + cmpAt(other, "ab", "abc") + " " + testAt(otherP, "", "v") + " "
                + testAt(otherP, "k", "v"));
        try {
            say("bigcmp cce " + cmpAt(c, "a", 1));
        } catch (Throwable t) {
            say("bigcmp cce " + t.getClass().getName());
        }
        try {
            say("bigcmp npe " + cmpAt(c, null, "a"));
        } catch (Throwable t) {
            say("bigcmp npe " + t.getClass().getName());
        }
        sum = 0;
        for (int i = 0; i < WARM; i++) {
            sum += cmpAt(c, WORDS[i % WORDS.length], WORDS[(i + 1) % WORDS.length]);
        }
        say("bigcmp rewarm " + sum);
    }

    static boolean unboxBool(Boolean b) {
        return b.booleanValue();
    }

    static char unboxChar(Character c) {
        return c.charValue();
    }

    // ---- bench -------------------------------------------------------------------------------

    static long tmap(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            TreeMap<String, String> m = new TreeMap<>(String.CASE_INSENSITIVE_ORDER);
            for (String s : NAMES) m.put(s, s);
            for (int k = 0; k < 6; k++) {
                String v = m.get(ASKS[k]);
                sum += v == null ? -1 : v.length();
            }
            for (Map.Entry<String, String> e : m.entrySet()) sum += e.getKey().charAt(0);
        }
        return sum;
    }

    static long tmapnatRow(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            TreeMap<String, String> m = new TreeMap<>();
            for (String s : NAMES) m.put(s, s);
            for (String s : NAMES) sum += m.get(s).length();
        }
        return sum;
    }

    static long cicmp(int n) {
        Comparator<String> c = String.CASE_INSENSITIVE_ORDER;
        long sum = 0;
        for (int i = 0; i < n; i++) {
            for (int k = 0; k < 6; k++) sum += c.compare(NAMES[k], ASKS[(k + i) % 6]);
        }
        return sum;
    }

    static long hdrRow(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            HttpHeaders h = HttpHeaders.of(RAW, (k, v) -> true);
            sum += h.firstValueAsLong("content-length").orElse(-1);
            sum += h.firstValue("CONNECTION").map(String::length).orElse(-1);
            sum += h.allValues("set-cookie").size();
            sum += h.firstValue("transfer-encoding").isPresent() ? 7 : 3;
            for (Map.Entry<String, List<String>> e : h.map().entrySet()) {
                sum += e.getKey().length() + e.getValue().size();
            }
        }
        return sum;
    }

    static long tmapcmpRow(int n) {
        long sum = 0;
        Comparator<String> byLen = new ByLength();
        for (int i = 0; i < n; i++) {
            TreeMap<String, String> m = new TreeMap<>(byLen);
            for (String s : NAMES) m.put(s, s);
            for (String s : NAMES) sum += m.get(s).length();
        }
        return sum;
    }

    static long boxRow(int n) {
        Boolean[] bs = {Boolean.TRUE, Boolean.FALSE};
        Character[] cs = {'a', 'b', 'c'};
        long sum = 0;
        for (int i = 0; i < n; i++) {
            for (int k = 0; k < 16; k++) {
                if (bs[k & 1].booleanValue()) sum += cs[k % 3].charValue();
            }
        }
        return sum;
    }

    static long tmapbigRow(int n) {
        long sum = 0;
        Comparator<Object> c = new CharOrder();
        for (int i = 0; i < n; i++) {
            TreeMap<String, String> m = new TreeMap<>(c);
            for (String s : NAMES) m.put(s, s);
            for (String s : NAMES) sum += m.get(s).length();
        }
        return sum;
    }

    static long eqicRow(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            for (int k = 0; k < 6; k++) {
                String x = NAMES[k];
                String y = ASKS[(k + i) % 6];
                if (x.equalsIgnoreCase(y)) sum += 3;
                if (x.regionMatches(true, 0, y, 0, 4)) sum += 1;
            }
        }
        return sum;
    }

    interface Row {
        long run(int n);
    }

    static long sink;

    static void time(String name, int iters, Row r) {
        if (!ROWS.equals("all") && !on(name)) return;
        int warm = Integer.getInteger("nio21.warm", 3);
        for (int w = 0; w < warm; w++) sink += r.run(iters);
        long[] ns = new long[5];
        for (int k = 0; k < ns.length; k++) {
            long t0 = System.nanoTime();
            sink += r.run(iters);
            ns[k] = (System.nanoTime() - t0) / iters;
        }
        Arrays.sort(ns);
        say(name + " " + ns[ns.length / 2] + " ns/op");
    }

    static void bench() {
        int n = Integer.getInteger("nio21.iters", 100_000);
        time("tmap", n, Nio21TreeMapRows::tmap);
        time("tmapnat", n, Nio21TreeMapRows::tmapnatRow);
        time("cicmp", n, Nio21TreeMapRows::cicmp);
        time("hdr", Math.max(1, n / 2), Nio21TreeMapRows::hdrRow);
        time("tmapcmp", n, Nio21TreeMapRows::tmapcmpRow);
        time("box", n, Nio21TreeMapRows::boxRow);
        time("tmapbig", n, Nio21TreeMapRows::tmapbigRow);
        time("eqic", n, Nio21TreeMapRows::eqicRow);
        System.err.println("sink " + sink);
    }
}
