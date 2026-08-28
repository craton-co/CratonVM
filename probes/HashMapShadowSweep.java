import java.util.*;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.function.Function;

/** The `java.util.HashMap` and `CopyOnWriteArrayList` triples the
 *  `--jdk-only-report` marks `outcome=native-won` — a bridge native that ran in
 *  front of real JDK bytecode rather than losing the dispatch to it.
 *
 *  Second family off the same worklist that produced ten defects in
 *  `java.util.Arrays`. The method that found them is repeated deliberately:
 *  ask the CONTRACT EDGES, not the happy path. A shim's middle is where it is
 *  most likely to be right; its refusals, null handling and bounds are where it
 *  was written from memory.
 *
 *    HashMap  <init>(), <init>(int), put, get, containsKey, getOrDefault,
 *             computeIfAbsent, isEmpty, keySet, values, entrySet
 *    COWList  add, addAll
 *
 *  `computeIfAbsent` carries the most contract per line in the JDK's map API and
 *  is asked hardest here: a null mapping function throws even when the key is
 *  present; a mapper returning null stores NOTHING and returns null; a mapper
 *  that throws propagates and leaves the map untouched; and a mapper that
 *  modifies the same map is a ConcurrentModificationException, not a corrupt
 *  table.
 *
 *  DETERMINISM: hash order is unspecified, so every collection read is sorted
 *  before printing, and no iteration order is asserted anywhere.
 */
public class HashMapShadowSweep {
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
    interface ThrowingRun { void run() throws Throwable; }
    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }

    // ---- <init>, put, get, containsKey, getOrDefault, isEmpty -----------
    static void basics() {
        HashMap<String, Integer> m = new HashMap<>();
        p("new isEmpty", m.isEmpty());
        p("new size", m.size());
        p("get absent", m.get("a"));
        p("containsKey absent", m.containsKey("a"));
        p("getOrDefault absent", m.getOrDefault("a", 99));

        p("put returns old (none)", m.put("a", 1));
        p("put returns old (present)", m.put("a", 2));
        p("get after put", m.get("a"));
        p("isEmpty after put", m.isEmpty());
        p("size after put", m.size());

        // NULL KEY and NULL VALUE are both legal in a HashMap, and this is the
        // pair a shim backed by a Rust map most often refuses or conflates.
        p("put null key", m.put(null, 7));
        p("get null key", m.get(null));
        p("containsKey null", m.containsKey(null));
        p("getOrDefault null key", m.getOrDefault(null, 99));
        p("size with null key", m.size());
        p("put null value", m.put("nv", null));
        p("get null value", m.get("nv"));
        // containsKey TRUE with get() null is the distinction a map that stores
        // "absent" as null cannot make.
        p("containsKey with null value", m.containsKey("nv"));
        p("getOrDefault with null value stored", m.getOrDefault("nv", 99));
        p("put null key twice returns old", m.put(null, 8));
        p("keys sorted", sorted(m.keySet()));

        // getOrDefault must NOT insert.
        HashMap<String, Integer> g = new HashMap<>();
        g.getOrDefault("zz", 1);
        p("getOrDefault does not insert", g.size());

        // Key equality is equals(), not identity.
        HashMap<String, Integer> e = new HashMap<>();
        e.put(new String("k"), 1);
        p("get by equal-but-not-same key", e.get(new String("k")));
        p("containsKey by equal key", e.containsKey(new String("k")));
        // and hashCode consistency across boxing
        HashMap<Integer, String> ints = new HashMap<>();
        ints.put(1000, "x");
        p("boxed Integer key beyond cache", ints.get(Integer.valueOf(1000)));

        t("new HashMap negative capacity", () -> new HashMap<String, Integer>(-1));
        t("new HashMap zero capacity", () -> new HashMap<String, Integer>(0));
        t("new HashMap huge capacity", () -> new HashMap<String, Integer>(1 << 30));
        t("new HashMap negative load", () -> new HashMap<String, Integer>(16, -1f));
        t("new HashMap NaN load", () -> new HashMap<String, Integer>(16, Float.NaN));
    }

    // ---- computeIfAbsent, the contract-dense one ------------------------
    static void computeIfAbsent() {
        HashMap<String, Integer> m = new HashMap<>();
        p("cIA computes", m.computeIfAbsent("a", k -> 1));
        p("cIA stored", m.get("a"));
        p("cIA present does not recompute", m.computeIfAbsent("a", k -> 999));
        p("cIA size", m.size());

        // A mapper returning null must store NOTHING and answer null.
        p("cIA mapper null result", m.computeIfAbsent("b", k -> null));
        p("cIA null result not stored", m.containsKey("b"));
        p("cIA size unchanged", m.size());

        // A null mapping function throws even when the key IS present.
        t("cIA null fn absent key", () -> m.computeIfAbsent("zz", null));
        t("cIA null fn present key", () -> m.computeIfAbsent("a", null));

        // The mapper sees the KEY it was called with.
        HashMap<String, String> k = new HashMap<>();
        k.computeIfAbsent("key", x -> "saw:" + x);
        p("cIA mapper receives key", k.get("key"));

        // A throwing mapper propagates and leaves the map untouched.
        HashMap<String, Integer> th = new HashMap<>();
        t("cIA mapper throws", () -> th.computeIfAbsent("t", x -> { throw new IllegalStateException("boom"); }));
        p("cIA map untouched after throw", th.size());
        p("cIA key absent after throw", th.containsKey("t"));

        // A null KEY is legal for computeIfAbsent.
        HashMap<String, Integer> nk = new HashMap<>();
        p("cIA null key computes", nk.computeIfAbsent(null, x -> 5));
        p("cIA null key stored", nk.get(null));

        // A key mapped to null is treated as ABSENT, so the mapper runs.
        HashMap<String, Integer> nv = new HashMap<>();
        nv.put("n", null);
        p("cIA over null value recomputes", nv.computeIfAbsent("n", x -> 3));
        p("cIA over null value stored", nv.get("n"));

        // A mapper that mutates the same map is a ConcurrentModificationException.
        HashMap<String, Integer> cme = new HashMap<>();
        cme.put("x", 1);
        t("cIA mapper mutates map", () -> cme.computeIfAbsent("y", x -> { cme.put("z", 9); return 2; }));
    }

    // ---- the three views, on the shadowed accessors ---------------------
    static void views() {
        HashMap<String, Integer> m = new HashMap<>();
        m.put("a", 1); m.put("b", 2);
        p("keySet sorted", sorted(m.keySet()));
        p("values sorted", sorted(m.values()));
        p("entrySet sorted", sorted(m.entrySet()));
        p("keySet size", m.keySet().size());
        p("values size", m.values().size());
        p("entrySet size", m.entrySet().size());
        // LIVE views: a put after the view is taken is visible through it.
        Set<String> ks = m.keySet();
        Collection<Integer> vs = m.values();
        Set<Map.Entry<String, Integer>> es = m.entrySet();
        m.put("c", 3);
        p("keySet is live", sorted(ks));
        p("values is live", sorted(vs));
        p("entrySet is live", es.size());
        // and a removal through the view writes back
        p("keySet remove writes through", ks.remove("a") + "/" + m.containsKey("a"));
        p("values remove writes through", vs.remove(2) + "/" + m.containsKey("b"));
        p("map size after view removals", m.size());
        // the same view object each time? (the JDK caches it)
        p("keySet is cached", m.keySet() == m.keySet());
        p("values is cached", m.values() == m.values());
        p("entrySet is cached", m.entrySet() == m.entrySet());
        // views of an EMPTY map
        HashMap<String, Integer> e = new HashMap<>();
        p("empty keySet", sorted(e.keySet()));
        p("empty values isEmpty", e.values().isEmpty());
        p("empty entrySet iterator hasNext", e.entrySet().iterator().hasNext());
    }

    // ---- CopyOnWriteArrayList add / addAll ------------------------------
    static void cow() {
        CopyOnWriteArrayList<String> l = new CopyOnWriteArrayList<>();
        p("cow add", l.add("a"));
        p("cow add duplicate allowed", l.add("a"));
        p("cow size", l.size());
        p("cow contents", l.toString());
        p("cow add null", l.add(null));
        p("cow contains null", l.contains(null));
        p("cow addAll", l.addAll(Arrays.asList("b", "c")));
        p("cow addAll empty returns false", l.addAll(new ArrayList<>()));
        p("cow contents after addAll", l.toString());
        p("cow addAll self", l.addAll(new ArrayList<>(l)));
        p("cow size after addAll self", l.size());
        t("cow addAll null", () -> l.addAll(null));
        // A COW iterator is a SNAPSHOT: a write during iteration is invisible to
        // it, and it must NOT throw ConcurrentModificationException.
        CopyOnWriteArrayList<String> s = new CopyOnWriteArrayList<>(Arrays.asList("1", "2"));
        Iterator<String> it = s.iterator();
        s.add("3");
        int n = 0;
        while (it.hasNext()) { it.next(); n++; }
        p("cow iterator is a snapshot", n);
        p("cow list saw the add", s.size());
        t("cow iterator remove unsupported", () -> {
            Iterator<String> q = s.iterator(); q.next(); q.remove();
        });
    }

    public static void main(String[] args) {
        basics();
        computeIfAbsent();
        views();
        cow();
        System.out.println("DONE HashMapShadowSweep");
    }
}
