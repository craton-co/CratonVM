import java.util.*;

/**
 * A shadow's disposition is decided by whether it diverges from the bytecode it
 * shadows.
 *
 * The census counts registrations standing in front of concrete JDK code
 * (contract §1.4 permits it), but counting them says nothing about whether any
 * of them is WRONG. This exercises the most-reached shadowed surface —
 * `java.util`'s immutable factories, `Map.entry`, and the views they hand out —
 * and prints every observable in `key=value` form so the whole run diffs
 * byte-for-byte against real HotSpot.
 *
 * Anything that differs is a shadow to fix or delete. Anything that matches is
 * a shadow with evidence behind it, which is what "adjudicated" has to mean for
 * a population this size.
 */
public class ShadowDifferentialProbe {

    static void line(String k, Object v) {
        System.out.println(k + "=" + v);
    }

    /** Same shape as the JDK's own contract tests: value, type, and identity. */
    static void entryLike(String tag, Map.Entry<?, ?> e) {
        line(tag + ".key", e.getKey());
        line(tag + ".value", e.getValue());
        line(tag + ".toString", e.toString());
        line(tag + ".hashCode.matchesSpec",
                e.hashCode() == (Objects.hashCode(e.getKey()) ^ Objects.hashCode(e.getValue())));
        line(tag + ".equalsSelf", e.equals(e));
        line(tag + ".equalsCopy", e.equals(Map.entry(e.getKey(), e.getValue())));
    }

    public static void main(String[] args) {
        // --- Map.entry --------------------------------------------------
        Map.Entry<String, Integer> me = Map.entry("k", 7);
        entryLike("Map.entry", me);
        try {
            me.setValue(9);
            line("Map.entry.setValue", "returned");
        } catch (Throwable t) {
            line("Map.entry.setValue", t.getClass().getName());
        }

        // --- the entry classes a user can construct directly ------------
        // `Map.entry` above is minted by a native; these two are what
        // ordinary bytecode reaches with `new`, and that is a different
        // allocation path (`num_total_fields` on the fabricated class, not
        // the slot count a native passes to `alloc_object`). It went unprobed
        // and was broken in synthetic mode: both slots read null.
        AbstractMap.SimpleEntry<String, Integer> se = new AbstractMap.SimpleEntry<>("k", 7);
        entryLike("new SimpleEntry", se);
        entryLike("new SimpleImmutableEntry", new AbstractMap.SimpleImmutableEntry<>("k", 7));
        line("new SimpleEntry.equalsEqualPeer", se.equals(new AbstractMap.SimpleEntry<>("k", 7)));
        line("new SimpleEntry.equalsSymmetric",
                new AbstractMap.SimpleEntry<>("k", 7).equals(se) == se.equals(new AbstractMap.SimpleEntry<>("k", 7)));
        se.setValue(8);
        line("new SimpleEntry.afterSetValue", se.getValue());
        try {
            new AbstractMap.SimpleImmutableEntry<>("k", 7).setValue(8);
            line("new SimpleImmutableEntry.setValue", "returned");
        } catch (Throwable t) {
            line("new SimpleImmutableEntry.setValue", t.getClass().getName());
        }

        // --- immutable factories ---------------------------------------
        List<String> l = List.of("a", "b", "c");
        line("List.of.toString", l);
        line("List.of.size", l.size());
        line("List.of.contains", l.contains("b"));
        line("List.of.indexOf", l.indexOf("c"));
        line("List.of.equalsArrayList", l.equals(new ArrayList<>(List.of("a", "b", "c"))));
        line("List.of.hashCodeMatchesArrayList",
                l.hashCode() == new ArrayList<>(List.of("a", "b", "c")).hashCode());
        try {
            l.add("d");
            line("List.of.add", "returned");
        } catch (Throwable t) {
            line("List.of.add", t.getClass().getName());
        }

        Set<String> s = Set.of("x", "y");
        line("Set.of.size", s.size());
        line("Set.of.contains", s.contains("y"));
        line("Set.of.equalsHashSet", s.equals(new HashSet<>(Set.of("x", "y"))));
        try {
            s.add("z");
            line("Set.of.add", "returned");
        } catch (Throwable t) {
            line("Set.of.add", t.getClass().getName());
        }

        Map<String, Integer> m = Map.of("p", 1, "q", 2);
        line("Map.of.size", m.size());
        line("Map.of.get", m.get("q"));
        line("Map.of.containsKey", m.containsKey("p"));
        line("Map.of.equalsHashMap", m.equals(new HashMap<>(Map.of("p", 1, "q", 2))));
        try {
            m.put("r", 3);
            line("Map.of.put", "returned");
        } catch (Throwable t) {
            line("Map.of.put", t.getClass().getName());
        }

        // Iteration order is unspecified for Set.of / Map.of, so sort.
        List<String> keys = new ArrayList<>(m.keySet());
        Collections.sort(keys);
        line("Map.of.sortedKeys", keys);
        List<String> setElems = new ArrayList<>(s);
        Collections.sort(setElems);
        line("Set.of.sortedElems", setElems);

        // --- entrySet of an ordinary map, and its entries ---------------
        Map<String, Integer> hm = new LinkedHashMap<>();
        hm.put("one", 1);
        hm.put("two", 2);
        StringBuilder sb = new StringBuilder();
        for (Map.Entry<String, Integer> e : hm.entrySet()) {
            sb.append(e).append(';');
        }
        line("LinkedHashMap.entrySet.toString", sb);
        Map.Entry<String, Integer> first = hm.entrySet().iterator().next();
        entryLike("LinkedHashMap.firstEntry", first);
        first.setValue(11);
        line("LinkedHashMap.afterSetValue", hm.get("one"));

        // --- unmodifiable views -----------------------------------------
        List<String> ul = Collections.unmodifiableList(new ArrayList<>(List.of("u", "v")));
        line("unmodifiableList.toString", ul);
        try {
            ul.add("w");
            line("unmodifiableList.add", "returned");
        } catch (Throwable t) {
            line("unmodifiableList.add", t.getClass().getName());
        }
        line("List.copyOf.toString", List.copyOf(new ArrayList<>(List.of("c1", "c2"))));
        line("Set.copyOf.size", Set.copyOf(new ArrayList<>(List.of("s1", "s2"))).size());
        line("Map.copyOf.size", Map.copyOf(new LinkedHashMap<>(Map.of("m1", 1))).size());

        // --- subList, a shadow the retag wave touched --------------------
        List<String> base = new ArrayList<>(List.of("s0", "s1", "s2", "s3"));
        List<String> sub = base.subList(1, 3);
        line("subList.toString", sub);
        line("subList.size", sub.size());
        sub.set(0, "CHANGED");
        line("subList.writeThrough", base);

        System.out.println("PROBE-DONE");
    }
}
