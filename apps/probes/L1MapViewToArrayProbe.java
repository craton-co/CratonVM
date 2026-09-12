import java.util.*;
import java.util.concurrent.Callable;

/**
 * L1 wave 7. The view surface of the four map families, and in particular the
 * ONE question that separated them from a retirement: what does `toArray()`
 * answer for a view the IMAGE'S OWN BYTECODE minted?
 *
 * Every row is order-normalised (sorted, or a count) because the four families
 * do not agree on iteration order and the point here is CONTENT, not order.
 * Where order IS the contract -- LinkedHashMap's insertion order, TreeMap's key
 * order -- there are explicit ordered rows in section E instead.
 */
public class L1MapViewToArrayProbe {

    static void p(String k, Object v) { System.out.println(k + " = " + v); }

    static String sorted(Object[] a) {
        String[] s = new String[a.length];
        for (int i = 0; i < a.length; i++) s[i] = String.valueOf(a[i]);
        Arrays.sort(s);
        return Arrays.toString(s);
    }

    static String sorted(Collection<?> c) { return sorted(c.toArray()); }

    static String t(Callable<?> c) {
        try { return String.valueOf(c.call()); }
        catch (Throwable x) { return "THREW " + x.getClass().getName(); }
    }

    static void views(final String tag, final Map<String, String> m) {
        p(tag + ".size", m.size());
        p(tag + ".entrySet.class", m.entrySet().getClass().getName());
        p(tag + ".keySet.class", m.keySet().getClass().getName());
        p(tag + ".values.class", m.values().getClass().getName());

        p(tag + ".entrySet.size", t(() -> m.entrySet().size()));
        p(tag + ".keySet.size", t(() -> m.keySet().size()));
        p(tag + ".values.size", t(() -> m.values().size()));

        p(tag + ".entrySet.toArray.len", t(() -> m.entrySet().toArray().length));
        p(tag + ".keySet.toArray.len", t(() -> m.keySet().toArray().length));
        p(tag + ".values.toArray.len", t(() -> m.values().toArray().length));

        p(tag + ".entrySet.toArray.sorted", t(() -> sorted(m.entrySet().toArray())));
        p(tag + ".keySet.toArray.sorted", t(() -> sorted(m.keySet().toArray())));
        p(tag + ".values.toArray.sorted", t(() -> sorted(m.values().toArray())));

        p(tag + ".keySet.toArrayTyped.sorted", t(() -> sorted((Object[]) m.keySet().toArray(new String[0]))));
        p(tag + ".entrySet.toArrayTyped.len", t(() -> m.entrySet().toArray(new Map.Entry[0]).length));
        p(tag + ".keySet.toArrayBig.len", t(() -> m.keySet().toArray(new String[10]).length));

        p(tag + ".entrySet.intoArrayList", t(() -> new ArrayList<Map.Entry<String, String>>(m.entrySet()).size()));
        p(tag + ".keySet.intoArrayList", t(() -> sorted(new ArrayList<String>(m.keySet()))));
        p(tag + ".keySet.intoHashSet", t(() -> sorted(new HashSet<String>(m.keySet()))));
        p(tag + ".keySet.intoTreeSet", t(() -> new TreeSet<String>(m.keySet()).toString()));
        p(tag + ".values.intoArrayList", t(() -> sorted(new ArrayList<String>(m.values()))));

        p(tag + ".keySet.addAllInto", t(() -> {
            List<String> out = new ArrayList<String>();
            out.addAll(m.keySet());
            return sorted(out);
        }));
        p(tag + ".entrySet.addAllInto", t(() -> {
            List<Map.Entry<String, String>> out = new ArrayList<Map.Entry<String, String>>();
            out.addAll(m.entrySet());
            return out.size();
        }));

        p(tag + ".keySet.containsAll", t(() -> m.keySet().containsAll(Arrays.asList("a", "b"))));
        p(tag + ".keySet.contains", t(() -> m.keySet().contains("b")));
        p(tag + ".entrySet.stream.count", t(() -> m.entrySet().stream().count()));
        p(tag + ".keySet.stream.sorted", t(() -> m.keySet().stream().sorted().toList().toString()));
        p(tag + ".entrySet.forLoop", t(() -> {
            int n = 0;
            for (Map.Entry<String, String> e : m.entrySet()) n++;
            return n;
        }));
        p(tag + ".entrySet.forLoop.sorted", t(() -> {
            List<String> out = new ArrayList<String>();
            for (Map.Entry<String, String> e : m.entrySet()) out.add(e.getKey() + "=" + e.getValue());
            Collections.sort(out);
            return out.toString();
        }));
        p(tag + ".keySet.iterator.class", t(() -> m.keySet().iterator().getClass().getName()));
        p(tag + ".entrySet.isEmpty", t(() -> m.entrySet().isEmpty()));
        p(tag + ".keySet.hashCodeEqualsSum", t(() -> {
            int sum = 0;
            for (String k : m.keySet()) sum += k.hashCode();
            return m.keySet().hashCode() == sum;
        }));
        p(tag + ".keySet.equalsSameKeys", t(() -> m.keySet().equals(new HashSet<String>(Arrays.asList("a", "b", "c")))));
        p(tag + ".entrySet.removeThrough", t(() -> {
            Map<String, String> copy = copyOf(m);
            Iterator<Map.Entry<String, String>> it = copy.entrySet().iterator();
            it.next();
            it.remove();
            return copy.size();
        }));
        p(tag + ".keySet.removeThrough", t(() -> {
            Map<String, String> copy = copyOf(m);
            copy.keySet().remove("b");
            return sorted(copy.keySet()) + " size=" + copy.size();
        }));
        p(tag + ".values.removeThrough", t(() -> {
            Map<String, String> copy = copyOf(m);
            copy.values().remove("2");
            return sorted(copy.keySet()) + " size=" + copy.size();
        }));
        p(tag + ".entrySet.setValue", t(() -> {
            Map<String, String> copy = copyOf(m);
            for (Map.Entry<String, String> e : copy.entrySet()) {
                if (e.getKey().equals("b")) e.setValue("22");
            }
            return copy.get("b");
        }));
    }

    static Map<String, String> copyOf(Map<String, String> m) {
        Map<String, String> out;
        if (m instanceof Hashtable) out = new Hashtable<String, String>();
        else if (m instanceof LinkedHashMap) out = new LinkedHashMap<String, String>();
        else if (m instanceof TreeMap) out = new TreeMap<String, String>();
        else out = new HashMap<String, String>();
        out.putAll(m);
        return out;
    }

    static <M extends Map<String, String>> M fill(M m) {
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");
        return m;
    }

    public static void main(String[] args) {
        views("A.hashtable", fill(new Hashtable<String, String>()));
        views("B.lhm", fill(new LinkedHashMap<String, String>()));
        views("C.hashmap", fill(new HashMap<String, String>()));
        views("D.treemap", fill(new TreeMap<String, String>()));

        LinkedHashMap<String, String> lhm = fill(new LinkedHashMap<String, String>());
        p("E.lhm.order.keys", new ArrayList<String>(lhm.keySet()).toString());
        p("E.lhm.order.entries", lhm.entrySet().toString());
        p("E.lhm.order.toArray", Arrays.toString(lhm.keySet().toArray()));
        TreeMap<String, String> tm = fill(new TreeMap<String, String>());
        p("E.treemap.order.keys", new ArrayList<String>(tm.keySet()).toString());
        p("E.treemap.order.toArray", Arrays.toString(tm.keySet().toArray()));
        p("E.treemap.descending.toArray", Arrays.toString(tm.descendingKeySet().toArray()));
        p("E.treemap.headMap.toArray", Arrays.toString(tm.headMap("c").keySet().toArray()));
        p("E.treemap.subMap.entries", tm.subMap("a", "c").entrySet().toString());

        Properties pr = new Properties();
        pr.setProperty("a", "1");
        pr.setProperty("b", "2");
        p("F.props.size", pr.size());
        p("F.props.keySet.toArray.sorted", sorted(pr.keySet().toArray()));
        p("F.props.entrySet.toArray.len", pr.entrySet().toArray().length);
        p("F.props.values.toArray.sorted", sorted(pr.values().toArray()));
        p("F.props.stringPropertyNames", new TreeSet<String>(pr.stringPropertyNames()).toString());

        Set<String> hs = new HashSet<String>(Arrays.asList("a", "b", "c"));
        p("G.hashset.toArray.sorted", sorted(hs.toArray()));
        Set<String> lhs = new LinkedHashSet<String>(Arrays.asList("a", "b", "c"));
        p("G.linkedhashset.toArray", Arrays.toString(lhs.toArray()));
        Set<String> ts = new TreeSet<String>(Arrays.asList("c", "a", "b"));
        p("G.treeset.toArray", Arrays.toString(ts.toArray()));

        Map<String, String> sync = Collections.synchronizedMap(fill(new HashMap<String, String>()));
        p("H.syncMap.keySet.toArray.sorted", sorted(sync.keySet().toArray()));
        p("H.syncMap.entrySet.toArray.len", sync.entrySet().toArray().length);
        Map<String, String> unmod = Collections.unmodifiableMap(fill(new HashMap<String, String>()));
        p("H.unmodMap.keySet.toArray.sorted", sorted(unmod.keySet().toArray()));
        p("H.unmodMap.entrySet.toArray.len", unmod.entrySet().toArray().length);
    }
}
