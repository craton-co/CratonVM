import java.util.*;
import java.util.function.Supplier;

/** L3 tail / the VIEW classes — 100 owning bridge-with-code registrations
 *  spread over eleven carrier classes:
 *
 *  ```text
 *  TreeMap$KeySet 24   LinkedHashMap$LinkedKeySet 9   HashMap$KeySet 9
 *  HashMap$Values 8    LinkedHashMap$LinkedValues 8   HashMap$EntrySet 7
 *  LinkedHashMap$LinkedEntrySet 7   TreeMap$EntrySet 6   TreeMap$Values 6
 *  Hashtable$EntrySet 6   Hashtable$KeySet 5   Hashtable$ValueCollection 4
 *  ```
 *
 *  A view is the one shape in `java.util` that has to be wrong in TWO
 *  directions before it looks wrong at all, which is why a probe that only
 *  reads one finds nothing:
 *
 *    * READ-THROUGH — a `put` into the source AFTER the view was taken must be
 *      visible in it. A defensive snapshot passes every mutation test and fails
 *      only this one;
 *    * WRITE-THROUGH — `keySet().remove(k)` deletes the ENTRY, `values()
 *      .remove(v)` deletes the first entry with that value, and
 *      `entrySet().removeIf` deletes whatever it matched. A view that owns its
 *      own storage reports success and changes nothing;
 *    * THE REFUSALS — `keySet().add` and `entrySet().add` are
 *      `UnsupportedOperationException` on every map family, because a key
 *      without a value is not an entry.
 *
 *  The same battery runs over five map families so a shared body that serves
 *  one of them cannot pass by serving the others. `Properties` is included
 *  because its views come from a different file again.
 *
 *  DETERMINISM: every hash-ordered view is sorted before printing; the TreeMap
 *  family's order IS its specification and is printed as encountered.
 */
public class MapViewsShadowSweep {
    static int rows = 0;
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
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }
    interface ThrowingRun { void run() throws Throwable; }
    static void t(String tag, ThrowingRun r) {
        try { r.run(); p(tag, "no-throw"); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }
    static String sorted(Collection<?> c) {
        List<String> l = new ArrayList<>();
        for (Object o : c) l.add(String.valueOf(o));
        Collections.sort(l);
        return l.toString();
    }
    static String sortedMap(Map<?, ?> m) {
        List<String> l = new ArrayList<>();
        for (Map.Entry<?, ?> e : m.entrySet()) l.add(e.getKey() + "=" + e.getValue());
        Collections.sort(l);
        return l.toString();
    }

    static Map<String, String> seed(Map<String, String> m) {
        m.put("a", "1");
        m.put("b", "2");
        m.put("c", "3");
        return m;
    }

    static void battery(String name, Supplier<Map<String, String>> make) {
        // ---- read-through -------------------------------------------------
        Map<String, String> m = seed(make.get());
        Set<String> ks = m.keySet();
        Collection<String> vs = m.values();
        Set<Map.Entry<String, String>> es = m.entrySet();
        p(name + " keySet", sorted(ks));
        p(name + " values", sorted(vs));
        p(name + " entrySet", sorted(es));
        p(name + " keySet size", ks.size());
        p(name + " values size", vs.size());
        p(name + " entrySet size", es.size());
        p(name + " keySet contains", ks.contains("a"));
        p(name + " keySet contains absent", ks.contains("zz"));
        p(name + " values contains", vs.contains("2"));
        p(name + " keySet isEmpty", ks.isEmpty());
        m.put("d", "4");
        p(name + " keySet saw a later put", sorted(ks));
        p(name + " values saw a later put", sorted(vs));
        p(name + " entrySet saw a later put", sorted(es));
        m.remove("d");
        p(name + " keySet saw a later remove", sorted(ks));

        // ---- the refusals -------------------------------------------------
        t(name + " keySet add", () -> m.keySet().add("z"));
        t(name + " values add", () -> m.values().add("9"));
        t(name + " entrySet add", () -> m.entrySet().add(
            new AbstractMap.SimpleEntry<>("z", "9")));
        t(name + " keySet addAll", () -> m.keySet().addAll(Arrays.asList("z")));

        // ---- write-through ------------------------------------------------
        Map<String, String> k1 = seed(make.get());
        p(name + " keySet.remove", k1.keySet().remove("a"));
        p(name + " keySet.remove wrote through", sortedMap(k1));
        p(name + " keySet.remove absent", k1.keySet().remove("zz"));

        Map<String, String> k2 = seed(make.get());
        Iterator<String> it = k2.keySet().iterator();
        it.next();
        it.remove();
        p(name + " keySet iterator remove size", k2.size());
        t(name + " keySet iterator remove twice", () -> it.remove());

        Map<String, String> k3 = seed(make.get());
        p(name + " keySet removeIf", k3.keySet().removeIf(x -> x.equals("b")));
        p(name + " keySet removeIf wrote through", sortedMap(k3));

        Map<String, String> k4 = seed(make.get());
        p(name + " keySet retainAll", k4.keySet().retainAll(Arrays.asList("a", "c")));
        p(name + " keySet retainAll wrote through", sortedMap(k4));

        Map<String, String> k5 = seed(make.get());
        p(name + " keySet removeAll", k5.keySet().removeAll(Arrays.asList("a")));
        p(name + " keySet removeAll wrote through", sortedMap(k5));

        Map<String, String> k6 = seed(make.get());
        k6.keySet().clear();
        p(name + " keySet clear emptied the map", k6.size());

        Map<String, String> v1 = seed(make.get());
        p(name + " values.remove", v1.values().remove("2"));
        p(name + " values.remove wrote through", sortedMap(v1));
        Map<String, String> v2 = seed(make.get());
        p(name + " values removeIf", v2.values().removeIf(x -> x.equals("3")));
        p(name + " values removeIf wrote through", sortedMap(v2));
        Map<String, String> v3 = seed(make.get());
        Iterator<String> vit = v3.values().iterator();
        vit.next();
        vit.remove();
        p(name + " values iterator remove size", v3.size());
        Map<String, String> v4 = seed(make.get());
        v4.values().clear();
        p(name + " values clear emptied the map", v4.size());

        Map<String, String> e1 = seed(make.get());
        for (Map.Entry<String, String> e : e1.entrySet()) {
            if (e.getKey().equals("a")) e.setValue("SET");
        }
        p(name + " entry setValue wrote through", sortedMap(e1));
        Map<String, String> e2 = seed(make.get());
        p(name + " entrySet removeIf", e2.entrySet().removeIf(e -> e.getValue().equals("2")));
        p(name + " entrySet removeIf wrote through", sortedMap(e2));
        Map<String, String> e3 = seed(make.get());
        Iterator<Map.Entry<String, String>> eit = e3.entrySet().iterator();
        eit.next();
        eit.remove();
        p(name + " entrySet iterator remove size", e3.size());
        Map<String, String> e4 = seed(make.get());
        e4.entrySet().clear();
        p(name + " entrySet clear emptied the map", e4.size());

        // ---- identity -----------------------------------------------------
        Map<String, String> i1 = seed(make.get());
        p(name + " keySet equals a HashSet",
            i1.keySet().equals(new HashSet<>(Arrays.asList("a", "b", "c"))));
        p(name + " keySet hashCode agrees",
            i1.keySet().hashCode() == new HashSet<>(Arrays.asList("a", "b", "c")).hashCode());
        p(name + " values equals a list (must be false)",
            i1.values().equals(Arrays.asList("1", "2", "3")));
        p(name + " entrySet equals a set of entries",
            i1.entrySet().equals(new HashSet<>(seed(new LinkedHashMap<>()).entrySet())));
        p(name + " keySet toArray length", i1.keySet().toArray().length);
        p(name + " values toArray length", i1.values().toArray().length);
        p(name + " entrySet toArray length", i1.entrySet().toArray().length);
        p(name + " keySet stream count", i1.keySet().stream().count());
        p(name + " values stream count", i1.values().stream().count());
        StringBuilder fe = new StringBuilder();
        List<String> seen = new ArrayList<>();
        i1.keySet().forEach(seen::add);
        Collections.sort(seen);
        p(name + " keySet forEach", seen.toString());
        p(name + " same keySet instance twice", i1.keySet().equals(i1.keySet()));

        // ---- fail-fast ----------------------------------------------------
        Map<String, String> f1 = seed(make.get());
        t(name + " keySet fail fast on put", () -> {
            for (String k : f1.keySet()) f1.put("z" + k, "9");
        });
        Map<String, String> f2 = seed(make.get());
        t(name + " entrySet fail fast on remove", () -> {
            for (Map.Entry<String, String> e : f2.entrySet()) f2.remove("a");
        });
        // A VALUE-replacing put is NOT structural and must not throw.
        Map<String, String> f3 = seed(make.get());
        t(name + " keySet not fail fast on a value put", () -> {
            for (String k : f3.keySet()) f3.put(k, "X");
        });
        p(name + " after the non-structural put", sortedMap(f3));

        // ---- empty --------------------------------------------------------
        Map<String, String> em = make.get();
        p(name + " empty keySet", sorted(em.keySet()));
        p(name + " empty values isEmpty", em.values().isEmpty());
        p(name + " empty entrySet iterator hasNext", em.entrySet().iterator().hasNext());
        t(name + " empty keySet iterator next", () -> em.keySet().iterator().next());
    }

    public static void main(String[] args) {
        battery("HashMap", HashMap::new);
        battery("LinkedHashMap", LinkedHashMap::new);
        battery("TreeMap", TreeMap::new);
        battery("Hashtable", Hashtable::new);
        battery("Properties", () -> {
            @SuppressWarnings({ "unchecked", "rawtypes" })
            Map<String, String> m = (Map<String, String>) (Map) new Properties();
            return m;
        });
        System.out.println("ROWS " + rows);
        System.out.println("DONE MapViewsShadowSweep");
    }
}
