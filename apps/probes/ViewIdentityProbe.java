import java.io.*;
import java.util.*;

/** L3 — the IDENTITY of a map's three views, measured on a binary at last.
 *
 *  Four records (C7-1, C13-1, C13-2, C13-3) describe this surface and every
 *  CratonVM row in all four is marked PREDICTED FROM SOURCE — "no CratonVM
 *  binary and no cargo were run". They are 18 days old and the lane has rewritten
 *  large parts of the view machinery since. `MapViewsShadowSweep` does not settle
 *  them either: it has 300 rows and not one of them asks a class name or an
 *  `instanceof`. It measures what a view DOES, never what it IS.
 *
 *  So this asks the identity questions those records predict answers to:
 *  the class, the supertypes, view caching, the `equals` contract that FOLLOWS
 *  from the supertype, and the mutator refusals.
 *
 *  `values().equals()` is the subtle one and the reason the supertype matters.
 *  `AbstractCollection` does NOT override `equals`, so two `values()` views of
 *  two EQUAL maps must compare FALSE — identity semantics. A view that is
 *  secretly a `List` or a `Set` would answer `true` there and look more correct
 *  while being less.
 */
public class ViewIdentityProbe {
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
        catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName() + " msg=" + esc(e.getMessage()));
        }
    }

    /** The full identity of one view object. */
    static void identity(String tag, Collection<?> v) {
        p(tag + " class", v.getClass().getName());
        p(tag + " superclass", v.getClass().getSuperclass() == null
                ? "null" : v.getClass().getSuperclass().getName());
        p(tag + " isSet", v instanceof Set);
        p(tag + " isList", v instanceof List);
        p(tag + " isCollection", v instanceof Collection);
        p(tag + " isRandomAccess", v instanceof RandomAccess);
        p(tag + " isSerializable", v instanceof Serializable);
        p(tag + " isHashSet", v instanceof HashSet);
        p(tag + " isArrayList", v instanceof ArrayList);
        p(tag + " isCloneable", v instanceof Cloneable);
    }

    /** The mutator refusals, which are a supertype consequence too. */
    static void refusals(String tag, Collection<Object> v) {
        tv(tag + " add", () -> { v.add("zz"); return "no-throw"; });
        tv(tag + " addAll", () -> { v.addAll(List.of("zz")); return "no-throw"; });
        tv(tag + " clear+restore", () -> "skipped");
    }

    static Map<String, Integer> seed(Map<String, Integer> m) {
        m.put("a", 1); m.put("b", 2);
        return m;
    }

    @SuppressWarnings("unchecked")
    static void family(String tag, Map<String, Integer> m) {
        identity(tag + " keySet", m.keySet());
        identity(tag + " values", m.values());
        identity(tag + " entrySet", m.entrySet());

        // View CACHING: the JDK returns the same object every time.
        p(tag + " keySet same twice", m.keySet() == m.keySet());
        p(tag + " values same twice", m.values() == m.values());
        p(tag + " entrySet same twice", m.entrySet() == m.entrySet());

        // The equals contract, which FOLLOWS from the supertype and is the
        // reason class identity is not cosmetic.
        Map<String, Integer> other = fresh(m);
        p(tag + " keySet equals equal-map keySet", m.keySet().equals(other.keySet()));
        p(tag + " values equals equal-map values", m.values().equals(other.values()));
        p(tag + " entrySet equals equal-map entrySet", m.entrySet().equals(other.entrySet()));
        p(tag + " values equals itself", m.values().equals(m.values()));
        p(tag + " keySet equals plain HashSet", m.keySet().equals(new HashSet<>(m.keySet())));
        p(tag + " values equals plain ArrayList",
                m.values().equals(new ArrayList<>(m.values())));
        p(tag + " keySet hashCode == set hashCode",
                m.keySet().hashCode() == new HashSet<>(m.keySet()).hashCode());
        tv(tag + " values hashCode is identity", () ->
                m.values().hashCode() != new ArrayList<>(m.values()).hashCode());

        // The casts these class names permit or refuse.
        tv(tag + " keySet cast to HashSet", () -> {
            HashSet<String> h = (HashSet<String>) m.keySet();
            return "ok size=" + h.size();
        });
        tv(tag + " values cast to ArrayList", () -> {
            ArrayList<Integer> a = (ArrayList<Integer>) m.values();
            return "ok size=" + a.size();
        });
        tv(tag + " values cast to List", () -> {
            List<Integer> a = (List<Integer>) m.values();
            return "ok size=" + a.size();
        });

        refusals(tag + " keySet", (Collection<Object>) (Collection<?>) m.keySet());
        refusals(tag + " values", (Collection<Object>) (Collection<?>) m.values());

        // Serialization is the sharpest consequence of the wrong supertype:
        // HotSpot's views are NOT Serializable and must refuse.
        tv(tag + " serialize keySet", () -> {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            try (ObjectOutputStream o = new ObjectOutputStream(b)) {
                o.writeObject(m.keySet());
            }
            return "wrote " + (b.toByteArray().length > 0);
        });
        tv(tag + " serialize values", () -> {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            try (ObjectOutputStream o = new ObjectOutputStream(b)) {
                o.writeObject(m.values());
            }
            return "wrote " + (b.toByteArray().length > 0);
        });
    }

    static Map<String, Integer> fresh(Map<String, Integer> m) {
        // No Properties branch: `family()` is never called with one, because a
        // Properties is a Map<Object,Object> and its views are asked directly
        // in main().
        if (m instanceof LinkedHashMap) return seed(new LinkedHashMap<>());
        if (m instanceof TreeMap) return seed(new TreeMap<>());
        if (m instanceof Hashtable) return seed(new Hashtable<>());
        return seed(new HashMap<>());
    }

    /** A view taken BEFORE a mutation must reflect it afterwards. */
    static void liveness(String tag, Map<String, Integer> m) {
        Set<String> ks = m.keySet();
        Collection<Integer> vs = m.values();
        Set<Map.Entry<String, Integer>> es = m.entrySet();
        m.put("c", 3);
        p(tag + " live keySet size", ks.size());
        p(tag + " live keySet has c", ks.contains("c"));
        p(tag + " live values size", vs.size());
        p(tag + " live values has 3", vs.contains(3));
        p(tag + " live entrySet size", es.size());
        m.remove("a");
        p(tag + " after remove keySet size", ks.size());
        p(tag + " after remove keySet has a", ks.contains("a"));
        p(tag + " after remove values size", vs.size());
        p(tag + " after remove entrySet size", es.size());
        // A value REPLACED in place moves no structural counter, which is the
        // case a generation-guarded cache gets wrong.
        m.put("b", 99);
        p(tag + " after replace values has 99", vs.contains(99));
        p(tag + " after replace values has 2", vs.contains(2));
        p(tag + " after replace values size", vs.size());
        // And the views are still the same objects they were.
        p(tag + " keySet still same", ks == m.keySet());
        p(tag + " values still same", vs == m.values());
    }

    public static void main(String[] args) {
        family("hm", seed(new HashMap<>()));
        family("lhm", seed(new LinkedHashMap<>()));
        family("tm", seed(new TreeMap<>()));
        family("ht", seed(new Hashtable<>()));

        Properties pr = new Properties();
        pr.setProperty("a", "1");
        pr.setProperty("b", "2");
        identity("props keySet", pr.keySet());
        identity("props values", pr.values());
        identity("props entrySet", pr.entrySet());
        p("props keySet same twice", pr.keySet() == pr.keySet());

        // A plain HashSet, whose iterator IS a HashMap$KeyIterator on HotSpot --
        // the chain C13-3 turns on.
        Set<String> hs = new HashSet<>(List.of("a", "b"));
        p("HashSet class", hs.getClass().getName());
        p("HashSet iterator class", hs.iterator().getClass().getName());
        p("hm keySet iterator class", seed(new HashMap<>()).keySet().iterator().getClass().getName());
        p("hm values iterator class", seed(new HashMap<>()).values().iterator().getClass().getName());
        p("hm entrySet iterator class",
                seed(new HashMap<>()).entrySet().iterator().getClass().getName());

        // A view taken from a map that is then MUTATED: still live, still the
        // same object, and its class must not change.
        Map<String, Integer> live = seed(new HashMap<>());
        Collection<Integer> lv = live.values();
        live.put("c", 3);
        p("live values after put class", lv.getClass().getName());
        p("live values after put size", lv.size());
        p("live values still same object", lv == live.values());

        // LIVENESS, which is the property a cached view must not cost. A view
        // held across a mutation has to answer for the map's CURRENT contents,
        // and a cache that hands back a stale snapshot is worse than no cache.
        // Asked of every family, because the caching decision differs per family.
        liveness("hm", seed(new HashMap<>()));
        liveness("lhm", seed(new LinkedHashMap<>()));
        liveness("tm", seed(new TreeMap<>()));
        liveness("ht", seed(new Hashtable<>()));

        System.out.println("DONE ViewIdentityProbe");
    }
}
