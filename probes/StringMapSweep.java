import java.util.*;
import java.util.concurrent.*;

/** java.lang.String, ConcurrentHashMap, Arrays and Collections --- the next
 *  families off the bridge-kind retirement surface, diffed against HotSpot.
 *
 *  NOT StringBuilder/StringBuffer/AbstractStringBuilder: that cluster has an
 *  open known-issue record with a diagnosed mechanism (WORKER-3-NOTE-3).
 *
 *  Deterministic only. CHM iteration order is unspecified, so every CHM read
 *  that could expose order is sorted before printing. Values are hex-escaped so
 *  the diff cannot depend on either VM's stdout encoding. */
public class StringMapSweep {
    static String esc(String s) {
        StringBuilder b = new StringBuilder(s.length());
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }
    static void p(String tag, Object v) {
        System.out.println(esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    interface ThrowingRun { void run() throws Exception; }

    static void strings() {
        String[] xs = {"", "a", "abc", "aBcAbC", "  pad  ", "a,b,,c,", "é中",
                       "line1\nline2", "aaa", "abcabc"};
        for (String s : xs) {
            String k = "[" + esc(s) + "]";
            p(k + " length", s.length());
            p(k + " hashCode", s.hashCode());
            p(k + " toUpperCase", s.toUpperCase(Locale.ROOT));
            p(k + " toLowerCase", s.toLowerCase(Locale.ROOT));
            p(k + " trim", "<" + s.trim() + ">");
            p(k + " strip", "<" + s.strip() + ">");
            p(k + " isBlank", s.isBlank());
            p(k + " chars.count", s.chars().count());
            p(k + " codePointCount", s.codePointCount(0, s.length()));
            p(k + " split(,)", Arrays.toString(s.split(",")));
            p(k + " split(,,-1)", Arrays.toString(s.split(",", -1)));
            p(k + " indexOf(b)", s.indexOf("b"));
            p(k + " lastIndexOf(b)", s.lastIndexOf("b"));
            p(k + " replace(a,Z)", s.replace("a", "Z"));
            p(k + " replaceAll(a+,Z)", s.replaceAll("a+", "Z"));
            p(k + " contains(bc)", s.contains("bc"));
            p(k + " startsWith(a)", s.startsWith("a"));
            p(k + " endsWith(c)", s.endsWith("c"));
            p(k + " repeat2", s.repeat(2));
            p(k + " bytes.len", s.getBytes(java.nio.charset.StandardCharsets.UTF_8).length);
            p(k + " toCharArray.len", s.toCharArray().length);
            p(k + " compareTo(abc)", Integer.signum(s.compareTo("abc")));
            p(k + " compareToIgnoreCase(ABC)", Integer.signum(s.compareToIgnoreCase("ABC")));
            p(k + " equalsIgnoreCase(ABC)", s.equalsIgnoreCase("ABC"));
            p(k + " lines.count", s.lines().count());
            p(k + " intern==self", s.intern() == s.intern());
        }
        p("join", String.join("-", "a", "b", "c"));
        p("valueOf(int)", String.valueOf(42));
        p("valueOf(double)", String.valueOf(1.5d));
        p("valueOf(null char[])", String.valueOf(new char[]{'h', 'i'}));
        p("format", String.format(Locale.ROOT, "%s|%d|%05.2f|%x|%b", "s", 7, 3.14159, 255, true));
        p("copyValueOf", String.copyValueOf(new char[]{'a', 'b'}));
        p("substring", "abcdef".substring(2, 4));
        p("concat", "ab".concat("cd"));
        p("matches", "abc123".matches("[a-z]+\\d+"));
        p("String.CASE_INSENSITIVE_ORDER", String.CASE_INSENSITIVE_ORDER.compare("A", "a"));
        t("substring oob", () -> "ab".substring(0, 9));
        t("charAt oob", () -> "ab".charAt(9));
        t("split null", () -> "ab".split(null));
        t("concat null", () -> "ab".concat(null));
    }

    static void chm() {
        ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
        for (int i = 0; i < 6; i++) m.put("k" + i, i);
        p("chm size", m.size());
        p("chm get k3", m.get("k3"));
        p("chm getOrDefault absent", m.getOrDefault("nope", -1));
        p("chm containsKey/Value", m.containsKey("k1") + "/" + m.containsValue(4));
        p("chm keys sorted", new TreeSet<>(m.keySet()));
        p("chm values sorted", new TreeSet<>(m.values()));
        p("chm entries sorted", new TreeMap<>(m));
        p("chm putIfAbsent existing", m.putIfAbsent("k1", 99));
        p("chm putIfAbsent new", m.putIfAbsent("k9", 9));
        p("chm replace", m.replace("k2", 22));
        p("chm replace(k,old,new) wrong old", m.replace("k2", 2, 222));
        p("chm remove(k,wrongV)", m.remove("k3", 999));
        p("chm remove(k)", m.remove("k3"));
        p("chm computeIfAbsent", m.computeIfAbsent("k10", k -> 10));
        p("chm computeIfPresent", m.computeIfPresent("k0", (k, v) -> v + 100));
        p("chm compute", m.compute("k1", (k, v) -> v == null ? 0 : v * 2));
        p("chm merge", m.merge("k1", 5, Integer::sum));
        p("chm after ops sorted", new TreeMap<>(m));
        p("chm isEmpty", m.isEmpty());
        p("chm equals HashMap copy", m.equals(new HashMap<>(m)));
        p("chm reduceValues", m.reduceValues(Long.MAX_VALUE, Integer::sum));
        p("chm search sorted-null", m.searchKeys(Long.MAX_VALUE, k -> null));
        m.clear();
        p("chm after clear", m.size() + "/" + m.isEmpty());
        t("chm null key", () -> new ConcurrentHashMap<String, Integer>().put(null, 1));
        t("chm null value", () -> new ConcurrentHashMap<String, Integer>().put("k", null));
        t("chm get null", () -> new ConcurrentHashMap<String, Integer>().get(null));
    }

    static void arraysAndCollections() {
        int[] a = {5, 3, 9, 1, 3};
        int[] b = a.clone();
        Arrays.sort(b);
        p("Arrays.sort", Arrays.toString(b));
        p("Arrays.binarySearch", Arrays.binarySearch(b, 5));
        p("Arrays.equals", Arrays.equals(a, a.clone()));
        p("Arrays.hashCode", Arrays.hashCode(a));
        p("Arrays.copyOf", Arrays.toString(Arrays.copyOf(a, 7)));
        p("Arrays.copyOfRange", Arrays.toString(Arrays.copyOfRange(a, 1, 3)));
        p("Arrays.fill", Arrays.toString(fill()));
        p("Arrays.stream.sum", Arrays.stream(a).sum());
        String[] s = {"b", "a", "C"};
        Arrays.sort(s);
        p("Arrays.sort(String)", Arrays.toString(s));
        Arrays.sort(s, String.CASE_INSENSITIVE_ORDER);
        p("Arrays.sort(cmp)", Arrays.toString(s));
        p("Arrays.asList", Arrays.asList(s));
        p("Arrays.deepToString", Arrays.deepToString(new Object[]{new int[]{1, 2}, "x"}));
        List<Integer> l = new ArrayList<>(Arrays.asList(3, 1, 2));
        Collections.sort(l);
        p("Collections.sort", l);
        p("Collections.max/min", Collections.max(l) + "/" + Collections.min(l));
        Collections.reverse(l);
        p("Collections.reverse", l);
        p("Collections.unmodifiable", Collections.unmodifiableList(l));
        p("Collections.emptyList", Collections.emptyList());
        p("Collections.singletonList", Collections.singletonList("x"));
        p("Collections.nCopies", Collections.nCopies(3, "y"));
        p("Collections.frequency", Collections.frequency(Arrays.asList(1, 1, 2), 1));
        p("Collections.binarySearch", Collections.binarySearch(Arrays.asList(1, 2, 3), 2));
        Collections.swap(l, 0, 2);
        p("Collections.swap", l);
        p("Collections.disjoint", Collections.disjoint(l, Arrays.asList(99)));
        t("unmodifiable add", () -> Collections.unmodifiableList(l).add(9));
        t("Arrays.asList add", () -> Arrays.asList("a").add("b"));
        t("binarySearch oob ok", () -> Arrays.binarySearch(new int[]{1}, 0, 1, 5));
        t("Collections.max empty", () -> Collections.max(new ArrayList<Integer>()));
    }
    static int[] fill() { int[] f = new int[4]; Arrays.fill(f, 6); return f; }

    public static void main(String[] a) {
        strings();
        chm();
        arraysAndCollections();
        System.out.println("DONE StringMapSweep");
    }
}
