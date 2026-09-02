import java.util.*;

/**
 * A WIDTH sweep of the `instanceof` / `Class.isInstance` agreement.
 *
 * Every row asks both questions of one object. On HotSpot the two columns can
 * never differ, so any `DIFF` row is a CratonVM defect and the sweep's job is
 * to say how many there are.
 *
 * Written to size the divergence behind
 * `Collections.unmodifiableList(x) instanceof RandomAccess` (false, while
 * `RandomAccess.class.isInstance(x)` was true) — and the answer was ONE row of
 * fourteen, which is what ruled out a general subtype-walk fault and pointed at
 * the per-instance display-class decision that turned out to be the cause. See
 * `RandomAccessProbe` for that one pair asked in depth, and
 * `W7-63-jca-advertise-vs-serve.md` §8d for the diagnosis.
 *
 * Keep the unrelated rows. A sweep whose every row is a suspect cannot tell you
 * that the fault is narrow.
 */
public final class InstanceofScope {
    public static void main(String[] a) {
        var al = new ArrayList<String>(); al.add("a");
        var ll = new LinkedList<String>(); ll.add("a");
        ask("String/CharSequence", "abc", CharSequence.class, "abc" instanceof CharSequence);
        ask("String/Comparable", "abc", Comparable.class, "abc" instanceof Comparable);
        ask("ArrayList/RandomAccess", al, RandomAccess.class, al instanceof RandomAccess);
        ask("ArrayList/Cloneable", al, Cloneable.class, al instanceof Cloneable);
        ask("LinkedList/Deque", ll, Deque.class, ll instanceof Deque);
        ask("unmodList/RandomAccess", Collections.unmodifiableList(al),
                RandomAccess.class, Collections.unmodifiableList(al) instanceof RandomAccess);
        ask("unmodList/List", Collections.unmodifiableList(al),
                List.class, Collections.unmodifiableList(al) instanceof List);
        ask("unmodSet/Set", Collections.unmodifiableSet(new HashSet<String>()),
                Set.class, Collections.unmodifiableSet(new HashSet<String>()) instanceof Set);
        ask("singletonList/RandomAccess", Collections.singletonList("a"),
                RandomAccess.class, Collections.singletonList("a") instanceof RandomAccess);
        ask("List.of/RandomAccess", List.of("a"),
                RandomAccess.class, List.of("a") instanceof RandomAccess);
        ask("Arrays.asList/RandomAccess", Arrays.asList("a"),
                RandomAccess.class, Arrays.asList("a") instanceof RandomAccess);
        ask("subList/RandomAccess", al.subList(0, 1),
                RandomAccess.class, al.subList(0, 1) instanceof RandomAccess);
        ask("Integer/Serializable", 1, java.io.Serializable.class,
                (Object) 1 instanceof java.io.Serializable);
        ask("HashMap/Cloneable", new HashMap<String, String>(), Cloneable.class,
                new HashMap<String, String>() instanceof Cloneable);
    }

    static void ask(String label, Object o, Class<?> iface, boolean byOpcode) {
        boolean byReflection = iface.isInstance(o);
        System.out.println((byOpcode == byReflection ? "OK   " : "DIFF ")
                + label + " | instanceof=" + byOpcode + " isInstance=" + byReflection
                + " class=" + o.getClass().getName());
    }
}
