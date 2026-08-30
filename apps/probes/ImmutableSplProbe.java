import java.util.*;

/** L3 — the immutable factories' spliterator characteristics, the whole matrix.
 *
 *  Four cells are open in compatible mode and the rule behind them has been
 *  guessed twice. "Size 1 immutable gets `Collections.singletonSpliterator`"
 *  explains `Set.of("a")`, `List.of("a")` and a one-entry `entrySet`, and is
 *  then contradicted by the SAME map's `keySet` and `values`, which are size 1
 *  and answer something else entirely.
 *
 *  So stop deriving from three cells and ask HotSpot for the shape. Every
 *  factory, at sizes 0, 1, 2 and 3, with the CLASS the view and the spliterator
 *  actually are printed beside the mask -- because a characteristics number is
 *  a claim about which class answered, and the class name says which.
 *
 *  The wrapper factories are the controls: `Collections.unmodifiableSet` really
 *  does delegate to the backing collection's spliterator, so it must NOT move.
 */
public class ImmutableSplProbe {
    static int rows = 0;

    interface Val { Object call() throws Throwable; }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + tag + " |" + String.valueOf(v) + "|");
    }

    static void tv(String tag, Val c) {
        try { p(tag, c.call()); }
        catch (Throwable e) { p(tag, "THREW " + e.getClass().getName()); }
    }

    /** Mask, decoded, plus the size the spliterator reports. A raw number that
     *  differs by one bit is unreadable; the decode names the bit. */
    static String decode(int c) {
        StringBuilder sb = new StringBuilder();
        if ((c & 0x0001) != 0) sb.append("DISTINCT,");
        if ((c & 0x0004) != 0) sb.append("SORTED,");
        if ((c & 0x0010) != 0) sb.append("ORDERED,");
        if ((c & 0x0040) != 0) sb.append("SIZED,");
        if ((c & 0x0100) != 0) sb.append("NONNULL,");
        if ((c & 0x0400) != 0) sb.append("IMMUTABLE,");
        if ((c & 0x1000) != 0) sb.append("CONCURRENT,");
        if ((c & 0x4000) != 0) sb.append("SUBSIZED,");
        return sb.toString();
    }

    static void coll(String tag, Collection<?> c) {
        Spliterator<?> s = c.spliterator();
        p(tag, c.size() + " " + s.characteristics() + " " + decode(s.characteristics())
                + " est=" + s.estimateSize());
    }

    static void cls(String tag, Collection<?> c) {
        p(tag + " cls", c.getClass().getName()
                + " / " + c.spliterator().getClass().getName());
    }

    public static void main(String[] args) {
        // ---- List.of at every size the JDK gives a distinct class to.
        coll("List.of()", List.of());
        coll("List.of(1)", List.of("a"));
        coll("List.of(2)", List.of("a", "b"));
        coll("List.of(3)", List.of("a", "b", "c"));
        cls("List.of()", List.of());
        cls("List.of(1)", List.of("a"));
        cls("List.of(2)", List.of("a", "b"));
        cls("List.of(3)", List.of("a", "b", "c"));

        // ---- Set.of, same ladder.
        coll("Set.of()", Set.of());
        coll("Set.of(1)", Set.of("a"));
        coll("Set.of(2)", Set.of("a", "b"));
        coll("Set.of(3)", Set.of("a", "b", "c"));
        cls("Set.of(1)", Set.of("a"));
        cls("Set.of(2)", Set.of("a", "b"));
        cls("Set.of(3)", Set.of("a", "b", "c"));

        // ---- The three views of an immutable map, at every size. This is the
        // block that breaks the size-1 rule, so it gets the most rows.
        Map<String, Integer> m0 = Map.of();
        Map<String, Integer> m1 = Map.of("a", 1);
        Map<String, Integer> m2 = Map.of("a", 1, "b", 2);
        Map<String, Integer> m3 = Map.of("a", 1, "b", 2, "c", 3);
        for (Object[] row : new Object[][] {
                {"Map.of()", m0}, {"Map.of(1)", m1}, {"Map.of(2)", m2}, {"Map.of(3)", m3}}) {
            @SuppressWarnings("unchecked")
            Map<String, Integer> m = (Map<String, Integer>) row[1];
            String t = (String) row[0];
            coll(t + " keySet", m.keySet());
            coll(t + " values", m.values());
            coll(t + " entrySet", m.entrySet());
        }
        cls("Map.of(1) keySet", m1.keySet());
        cls("Map.of(1) values", m1.values());
        cls("Map.of(1) entrySet", m1.entrySet());
        cls("Map.of(3) keySet", m3.keySet());
        cls("Map.of(3) values", m3.values());
        cls("Map.of(3) entrySet", m3.entrySet());

        // ---- Map.ofEntries and copyOf reach the same classes by another door.
        coll("Map.ofEntries(1) keySet", Map.ofEntries(Map.entry("a", 1)).keySet());
        coll("Map.ofEntries(1) entrySet", Map.ofEntries(Map.entry("a", 1)).entrySet());
        coll("Map.copyOf(1) keySet", Map.copyOf(new HashMap<>(m1)).keySet());
        coll("List.copyOf(1)", List.copyOf(new ArrayList<>(List.of("a"))));
        coll("Set.copyOf(1)", Set.copyOf(new ArrayList<>(List.of("a"))));

        // ---- CONTROLS. The singleton/empty family, whose masks the rule was
        // read off, and the WRAPPER family, which must keep delegating.
        coll("Collections.singleton", Collections.singleton("a"));
        coll("Collections.singletonList", Collections.singletonList("a"));
        coll("Collections.singletonMap keySet", Collections.singletonMap("a", 1).keySet());
        coll("Collections.singletonMap values", Collections.singletonMap("a", 1).values());
        coll("Collections.singletonMap entrySet", Collections.singletonMap("a", 1).entrySet());
        coll("Collections.emptySet", Collections.emptySet());
        coll("Collections.emptyList", Collections.emptyList());
        coll("Collections.emptyMap keySet", Collections.emptyMap().keySet());

        Set<String> hs = new LinkedHashSet<>(List.of("a"));
        List<String> al = new ArrayList<>(List.of("a"));
        Map<String, Integer> hm = new LinkedHashMap<>(m1);
        coll("unmodifiableSet(1)", Collections.unmodifiableSet(hs));
        coll("unmodifiableList(1)", Collections.unmodifiableList(al));
        coll("unmodifiableMap keySet", Collections.unmodifiableMap(hm).keySet());
        coll("unmodifiableMap values", Collections.unmodifiableMap(hm).values());
        coll("unmodifiableMap entrySet", Collections.unmodifiableMap(hm).entrySet());
        cls("unmodifiableSet(1)", Collections.unmodifiableSet(hs));
        cls("unmodifiableMap keySet", Collections.unmodifiableMap(hm).keySet());

        // ---- And the mutable baselines the views are compared against, so a
        // wrong immutable answer cannot be blamed on the family's own bits.
        coll("LinkedHashSet(1)", hs);
        coll("ArrayList(1)", al);
        coll("LinkedHashMap keySet", hm.keySet());
        coll("LinkedHashMap values", hm.values());
        coll("LinkedHashMap entrySet", hm.entrySet());

        System.out.println("DONE ImmutableSplProbe");
    }
}
