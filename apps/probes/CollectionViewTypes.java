import java.io.Serializable;
import java.util.*;
import java.util.concurrent.*;

/**
 * Every collection VIEW this VM hands back, asked what it is.
 *
 * In real-JDK mode these are real classes and the answers come from real class
 * files. In `--synthetic-jdk` there is no class file, so each one's supertype
 * set comes from `class_manager.rs`'s interface table — and a name with no arm
 * there falls to `_ => &[]` and declares NOTHING, not even `Collection`.
 *
 * That default is invisible from inside the VM: `getClass()` still names the
 * right class and `getInterfaces()` still answers, so only a type QUESTION
 * shows it. Diff this against HotSpot; every row must match in all three modes.
 *
 * `RandomAccess` is the column that matters most and reads as decoration:
 * `Collections.binarySearch`, `reverse`, `shuffle` and `fill` each branch on it
 * to choose indexed access over an iterator, and `Collections.unmodifiableList`
 * picks its view CLASS by it.
 */
public final class CollectionViewTypes {
    public static void main(String[] args) {
        List<String> al = new ArrayList<>(List.of("a", "b"));
        List<String> ll = new LinkedList<>(List.of("a", "b"));
        Set<String> hs = new LinkedHashSet<>(Set.of("a"));
        SortedSet<String> ts = new TreeSet<>(Set.of("a"));
        Map<String, String> hm = new LinkedHashMap<>(Map.of("a", "b"));

        row("unmodifiableList(ArrayList)", () -> Collections.unmodifiableList(al));
        row("unmodifiableList(LinkedList)", () -> Collections.unmodifiableList(ll));
        row("unmodifiableCollection", () -> Collections.unmodifiableCollection(al));
        row("unmodifiableSet", () -> Collections.unmodifiableSet(hs));
        row("unmodifiableSortedSet", () -> Collections.unmodifiableSortedSet(ts));
        row("unmodifiableMap", () -> Collections.unmodifiableMap(hm));
        row("Arrays.asList", () -> Arrays.asList("a", "b"));
        row("List.of/2", () -> List.of("a", "b"));
        row("List.of/3", () -> List.of("a", "b", "c"));
        row("Set.of/1", () -> Set.of("a"));
        row("Set.of/3", () -> Set.of("a", "b", "c"));
        row("Map.of/1", () -> Map.of("a", "b"));
        row("Map.of/2", () -> Map.of("a", "b", "c", "d"));
        row("singletonList", () -> Collections.singletonList("a"));
        row("singleton", () -> Collections.singleton("a"));
        row("singletonMap", () -> Collections.singletonMap("a", "b"));
        row("emptyList", () -> Collections.emptyList());
        row("emptySet", () -> Collections.emptySet());
        row("emptyMap", () -> Collections.emptyMap());
        row("ArrayList.subList", () -> al.subList(0, 1));
        row("unmodifiableNavigableSet", () -> Collections.unmodifiableNavigableSet(new TreeSet<>(Set.of("a"))));
        row("unmodifiableSortedMap", () -> Collections.unmodifiableSortedMap(new TreeMap<>(Map.of("a", "b"))));
        // Controls: the concrete classes the views wrap. The SORTED ones are
        // here because a group in the interface table costs its members exactly
        // the markers that distinguish them, and these are the members.
        row("ArrayList", () -> al);
        row("LinkedList", () -> ll);
        row("Vector", () -> new Vector<>(List.of("a")));
        row("CopyOnWriteArrayList", () -> new CopyOnWriteArrayList<>(List.of("a")));
        row("TreeSet", () -> ts);
        row("ConcurrentSkipListSet", () -> new ConcurrentSkipListSet<>(Set.of("a")));
        row("EnumSet", () -> EnumSet.of(java.time.DayOfWeek.MONDAY));
        row("CopyOnWriteArraySet", () -> new CopyOnWriteArraySet<>(Set.of("a")));
        row("LinkedHashMap", () -> hm);
        row("TreeMap", () -> new TreeMap<>(Map.of("a", "b")));
        row("ConcurrentSkipListMap", () -> new ConcurrentSkipListMap<>(Map.of("a", "b")));
        row("ArrayDeque", () -> new ArrayDeque<>(List.of("a")));
        row("PriorityQueue", () -> new PriorityQueue<>(List.of("a")));
    }

    static final Class<?>[] ASKED = {
        Collection.class, List.class, Set.class, SortedSet.class, NavigableSet.class,
        Map.class, SortedMap.class, NavigableMap.class, Queue.class, Deque.class,
        Iterable.class, RandomAccess.class, Serializable.class, Cloneable.class,
    };

    /**
     * Each row is built behind a supplier and each failure is PRINTED rather
     * than thrown. The first version called the factories inline: in
     * `--synthetic-jdk`, `Collections.unmodifiableSortedMap` raised
     * `NoSuchMethodError` at row 22 and the process died, so the fourteen
     * control rows below it — every sorted/navigable class, the whole reason
     * they are in this probe — silently measured nothing. A probe that stops
     * early does not report less, it reports a shorter file that still diffs.
     */
    static void row(String label, java.util.function.Supplier<Object> make) {
        Object v;
        try {
            v = make.get();
        } catch (Throwable t) {
            System.out.println(label + " | ERROR " + t.getClass().getName() + ": " + t.getMessage());
            return;
        }
        row(label, v);
    }

    static void row(String label, Object v) {
        StringBuilder sb = new StringBuilder(label).append(" |");
        for (Class<?> c : ASKED) {
            // Both doors, because they can disagree and a row that asked only
            // one would not say which.
            boolean op = c.isInstance(v);
            boolean cast;
            try {
                c.cast(v);
                cast = true;
            } catch (ClassCastException e) {
                cast = false;
            }
            sb.append(' ').append(c.getSimpleName()).append('=').append(op)
              .append(op == cast ? "" : "/CAST=" + cast);
        }
        System.out.println(sb.append(" | class=").append(v.getClass().getName()));
    }
}
