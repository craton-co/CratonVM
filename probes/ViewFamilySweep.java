import java.util.*;
import java.util.concurrent.*;

/** EVERY map view class on the retirement surface, exhaustively: keySet(),
 *  values() and entrySet() for HashMap, LinkedHashMap, TreeMap, Hashtable and
 *  ConcurrentHashMap.
 *
 *  This is aimed by the survey's own prior. The one view defect found so far --
 *  `treeMap.keySet().add()` succeeding because native-collections mirrors the
 *  whole TreeSet surface onto TreeMap$KeySet -- was a family whose registrar
 *  CLAIMED an identical surface. Every class below is under the same kind of
 *  claim, so this asks each of them the questions where a view and its backing
 *  collection have OPPOSITE contracts:
 *
 *    * add / addAll must be refused on all three view kinds
 *    * remove / removeAll / retainAll / clear must WRITE THROUGH
 *    * Entry.setValue must write through
 *    * iterator().remove() must write through
 *    * the view must be LIVE: a later put is visible through it
 *
 *  Hash order is unspecified, so every read is sorted before printing. */
public class ViewFamilySweep {
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

    static Map<String, Integer> fresh(String kind) {
        Map<String, Integer> m;
        switch (kind) {
            case "HashMap": m = new HashMap<>(); break;
            case "LinkedHashMap": m = new LinkedHashMap<>(); break;
            case "TreeMap": m = new TreeMap<>(); break;
            case "Hashtable": m = new Hashtable<>(); break;
            case "ConcurrentHashMap": m = new ConcurrentHashMap<>(); break;
            default: throw new IllegalArgumentException(kind);
        }
        m.put("a", 1); m.put("b", 2); m.put("c", 3);
        return m;
    }
    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }

    static void views(String kind) {
        String k = "[" + kind + "]";

        // ---- keySet ------------------------------------------------------
        Map<String, Integer> m = fresh(kind);
        Set<String> ks = m.keySet();
        p(k + " keySet sorted", sorted(ks));
        p(k + " keySet size", ks.size());
        p(k + " keySet contains", ks.contains("b"));
        t(k + " keySet add", () -> ks.add("z"));
        t(k + " keySet addAll", () -> ks.addAll(Arrays.asList("y")));
        m.put("d", 4);
        p(k + " keySet is LIVE after put", sorted(ks));
        p(k + " keySet remove writes through", ks.remove("a") + "/" + sorted(m.keySet()));
        Iterator<String> it = ks.iterator();
        it.next(); it.remove();
        p(k + " keySet iterator.remove size", m.size());
        p(k + " keySet removeAll", ks.removeAll(Arrays.asList("b", "c")) + "/" + sorted(m.keySet()));

        // ---- values ------------------------------------------------------
        Map<String, Integer> m2 = fresh(kind);
        Collection<Integer> vs = m2.values();
        p(k + " values sorted", sorted(vs));
        p(k + " values size", vs.size());
        p(k + " values contains", vs.contains(2));
        t(k + " values add", () -> vs.add(9));
        m2.put("d", 4);
        p(k + " values is LIVE after put", sorted(vs));
        p(k + " values remove writes through", vs.remove(Integer.valueOf(1)) + "/" + m2.size());
        Iterator<Integer> vi = vs.iterator();
        vi.next(); vi.remove();
        p(k + " values iterator.remove size", m2.size());

        // ---- entrySet ----------------------------------------------------
        Map<String, Integer> m3 = fresh(kind);
        Set<Map.Entry<String, Integer>> es = m3.entrySet();
        p(k + " entrySet sorted", sorted(es));
        p(k + " entrySet size", es.size());
        t(k + " entrySet add", () -> es.add(Map.entry("z", 9)));
        Map.Entry<String, Integer> one = null;
        for (Map.Entry<String, Integer> e : es) if (e.getKey().equals("b")) one = e;
        p(k + " entry getKey/getValue", one == null ? "null" : one.getKey() + "=" + one.getValue());
        if (one != null) {
            final Map.Entry<String, Integer> fe = one;
            t(k + " entry setValue", () -> fe.setValue(22));
            p(k + " entry setValue writes through", m3.get("b"));
        }
        m3.put("d", 4);
        p(k + " entrySet is LIVE after put", es.size());
        Iterator<Map.Entry<String, Integer>> ei = es.iterator();
        ei.next(); ei.remove();
        p(k + " entrySet iterator.remove size", m3.size());
        p(k + " entrySet clear writes through", clearIt(es, m3));

        // ---- equality and emptiness --------------------------------------
        Map<String, Integer> m4 = fresh(kind);
        p(k + " keySet equals HashSet", m4.keySet().equals(new HashSet<>(Arrays.asList("a","b","c"))));
        p(k + " entrySet equals copy", m4.entrySet().equals(new HashMap<>(m4).entrySet()));
        m4.clear();
        p(k + " views empty after map clear",
          m4.keySet().isEmpty() + "/" + m4.values().isEmpty() + "/" + m4.entrySet().isEmpty());
    }
    static String clearIt(Set<?> view, Map<?, ?> backing) {
        try { view.clear(); return "cleared, map size " + backing.size(); }
        catch (Throwable t) { return "THREW " + t.getClass().getName(); }
    }

    public static void main(String[] a) {
        for (String kind : new String[]{"HashMap", "LinkedHashMap", "TreeMap",
                                        "Hashtable", "ConcurrentHashMap"}) {
            views(kind);
        }
        System.out.println("DONE ViewFamilySweep");
    }
}
