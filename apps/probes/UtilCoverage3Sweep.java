import java.util.*;

/** L3 coverage sweep, round 3 — the map-view methods still unreached.
 *
 *  Rounds 1 and 2 took the corpus from 445 of 602 owning registrations to 546.
 *  The 56 left are mostly one shape: `entrySet().contains/remove` (which take a
 *  `Map.Entry` rather than a key), the typed `toArray(T[])` of every view, and
 *  `forEach` on the views. Plus the `TimeZone` offset pair.
 *
 *  `entrySet().remove(entry)` is the interesting one and the reason this exists:
 *  it must match on BOTH key and value, and it must write through to the source
 *  map. A view that matched on the key alone would delete an entry the caller
 *  did not name.
 */
public class UtilCoverage3Sweep {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static String esc(String s) {
        return s == null ? "null" : s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    static Map.Entry<String, Integer> entry(String k, Integer v) {
        return new AbstractMap.SimpleEntry<>(k, v);
    }

    /** The whole view surface for one map, so the four families answer the same
     *  questions and a difference is attributable to the family. */
    static void views(String tag, Map<String, Integer> m) {
        Set<Map.Entry<String, Integer>> es = m.entrySet();
        p(tag + " es contains match", es.contains(entry("a", 1)));
        p(tag + " es contains wrong value", es.contains(entry("a", 99)));
        p(tag + " es contains absent key", es.contains(entry("zz", 1)));
        p(tag + " es contains non-entry", es.contains("a"));
        p(tag + " es contains null", es.contains(null));

        StringBuilder fe = new StringBuilder();
        new TreeMap<>(m).entrySet().forEach(e -> fe.append(e.getKey()).append('=')
                .append(e.getValue()).append(','));
        p(tag + " es forEach", fe.toString());

        StringBuilder fv = new StringBuilder();
        new TreeMap<>(m).values().forEach(v -> fv.append(v).append(','));
        p(tag + " values forEach", fv.toString());

        p(tag + " ks toArray typed", Arrays.toString(
                new TreeSet<>(m.keySet()).toArray(new String[0])));
        p(tag + " ks toArray typed big", new TreeSet<>(m.keySet())
                .toArray(new String[6]).length);
        p(tag + " values toArray typed", Arrays.toString(
                new TreeSet<>(m.values()).toArray(new Integer[0])));

        // remove(Object) on an entrySet: matches on key AND value, and writes
        // through. Done on a fresh copy so the earlier rows are unaffected.
        Map<String, Integer> r1 = fresh(m);
        p(tag + " es remove wrong value", r1.entrySet().remove(entry("a", 99)));
        p(tag + " es after wrong value", new TreeMap<>(r1).toString());
        Map<String, Integer> r2 = fresh(m);
        p(tag + " es remove match", r2.entrySet().remove(entry("a", 1)));
        p(tag + " es after match", new TreeMap<>(r2).toString());
        Map<String, Integer> r3 = fresh(m);
        p(tag + " es remove absent", r3.entrySet().remove(entry("zz", 1)));
        Map<String, Integer> r4 = fresh(m);
        p(tag + " es remove non-entry", r4.entrySet().remove("a"));
    }

    static Map<String, Integer> fresh(Map<String, Integer> m) {
        if (m instanceof LinkedHashMap) return new LinkedHashMap<>(m);
        if (m instanceof TreeMap) return new TreeMap<>(m);
        if (m instanceof Hashtable) { Hashtable<String, Integer> h = new Hashtable<>(); h.putAll(m); return h; }
        return new HashMap<>(m);
    }

    static Map<String, Integer> seed(Map<String, Integer> m) {
        m.put("a", 1);
        m.put("b", 2);
        m.put("c", 3);
        return m;
    }

    public static void main(String[] args) {
        views("hm", seed(new HashMap<>()));
        views("lhm", seed(new LinkedHashMap<>()));
        views("tm", seed(new TreeMap<>()));
        views("ht", seed(new Hashtable<>()));

        // HashSet's own typed toArray, and Vector.addAll(Collection).
        Set<String> hs = new HashSet<>(Arrays.asList("a", "b"));
        p("hs toArray typed", Arrays.toString(new TreeSet<>(hs).toArray(new String[0])));
        p("hs toArray typed big last", new TreeSet<>(hs).toArray(new String[4])[3]);
        Vector<String> v = new Vector<>(Arrays.asList("a"));
        p("Vector.addAll", v.addAll(Arrays.asList("b", "c")));
        p("Vector after addAll", v.toString());
        p("Vector.addAll empty", v.addAll(new ArrayList<String>()));

        // LinkedHashMap.entrySet().reversed()
        LinkedHashMap<String, Integer> lr = new LinkedHashMap<>();
        lr.put("a", 1); lr.put("b", 2); lr.put("c", 3);
        // `entrySet()` is declared `Set`, so the SequencedSet method needs the
        // sequenced static type to be visible.
        SequencedSet<Map.Entry<String, Integer>> les = lr.sequencedEntrySet();
        tv("lhm es reversed", () -> les.reversed().toString());

        // The TimeZone offset pair.
        TimeZone ny = TimeZone.getTimeZone("America/New_York");
        p("TZ getOffset(epoch)", ny.getOffset(0L));
        p("TZ getOffset(summer)", ny.getOffset(1750000000000L));
        p("TZ getDefault non-null", TimeZone.getDefault() != null);
        p("TZ getDisplayName(en)", ny.getDisplayName(Locale.ENGLISH));
        p("TZ utc getOffset", TimeZone.getTimeZone("UTC").getOffset(0L));

        System.out.println("DONE UtilCoverage3Sweep");
    }
}
