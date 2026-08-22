import java.io.*;
import java.util.*;

/**
 * Round-trips every JDK collection wrapper CratonVM mints itself through Java
 * serialization and self-checks the result against the value that went in.
 *
 * The shapes that matter are the ones whose real JDK class declares a
 * `writeReplace()`: that body reads the class's OWN declared fields, which a
 * CratonVM-minted carrier does not populate (its state lives in this VM's own
 * slots), so the replacement object it hands the stream is built from nulls.
 * See probes/CollectionSerProbe.expected.txt for the HotSpot oracle.
 */
public class CollectionSerProbe {
    static int fails = 0;

    static byte[] ser(Object o) throws Exception {
        ByteArrayOutputStream b = new ByteArrayOutputStream();
        try (ObjectOutputStream os = new ObjectOutputStream(b)) { os.writeObject(o); }
        return b.toByteArray();
    }

    static Object deser(byte[] b) throws Exception {
        try (ObjectInputStream is = new ObjectInputStream(new ByteArrayInputStream(b))) {
            return is.readObject();
        }
    }

    /** Order-independent rendering, so a hash-order difference is not a failure. */
    static String norm(Object o) {
        if (o instanceof Map<?, ?> m) return new TreeMap<Object, Object>(m).toString();
        if (o instanceof Set<?> s) return new TreeSet<Object>(s).toString();
        if (o instanceof Collection<?> c) return c.toString();
        return String.valueOf(o);
    }

    static void rt(String name, Object o) {
        String want = norm(o);
        try {
            Object back = deser(ser(o));
            String got = norm(back);
            boolean ok = got.equals(want);
            if (!ok) fails++;
            System.out.println((ok ? "OK   " : "FAIL ") + name + " => " + got
                    + (ok ? "" : "  (want " + want + ")"));
        } catch (Throwable t) {
            fails++;
            StringBuilder sb = new StringBuilder(t.toString());
            for (Throwable c = t.getCause(); c != null; c = c.getCause()) sb.append(" <- ").append(c);
            System.out.println("FAIL " + name + " threw " + sb);
        }
    }

    public static void main(String[] args) {
        List<String> src3 = new ArrayList<>(Arrays.asList("a", "b", "c"));
        Map<String, String> msrc = new LinkedHashMap<>();
        msrc.put("a", "1"); msrc.put("b", "2"); msrc.put("c", "3");

        System.out.println("== List.of / Set.of / Map.of");
        rt("List.of()", List.of());
        rt("List.of(1)", List.of("a"));
        rt("List.of(2)", List.of("a", "b"));
        rt("List.of(3)", List.of("a", "b", "c"));
        rt("List.of(5)", List.of("a", "b", "c", "d", "e"));
        rt("Set.of()", Set.of());
        rt("Set.of(1)", Set.of("a"));
        rt("Set.of(2)", Set.of("a", "b"));
        rt("Set.of(3)", Set.of("a", "b", "c"));
        rt("Set.of(5)", Set.of("a", "b", "c", "d", "e"));
        rt("Map.of()", Map.of());
        rt("Map.of(1)", Map.of("a", "1"));
        rt("Map.of(2)", Map.of("a", "1", "b", "2"));
        rt("Map.of(3)", Map.of("a", "1", "b", "2", "c", "3"));
        rt("Map.ofEntries(2)", Map.ofEntries(Map.entry("a", "1"), Map.entry("b", "2")));

        System.out.println("== copyOf");
        rt("List.copyOf", List.copyOf(src3));
        rt("Set.copyOf", Set.copyOf(src3));
        rt("Map.copyOf", Map.copyOf(msrc));

        System.out.println("== Collections.unmodifiable*");
        rt("unmodifiableList", Collections.unmodifiableList(src3));
        rt("unmodifiableList(LinkedList)", Collections.unmodifiableList(new LinkedList<>(src3)));
        rt("unmodifiableCollection", Collections.unmodifiableCollection(src3));
        rt("unmodifiableSet", Collections.unmodifiableSet(new LinkedHashSet<>(src3)));
        rt("unmodifiableSortedSet", Collections.unmodifiableSortedSet(new TreeSet<>(src3)));
        rt("unmodifiableNavigableSet", Collections.unmodifiableNavigableSet(new TreeSet<>(src3)));
        rt("unmodifiableMap", Collections.unmodifiableMap(msrc));
        rt("unmodifiableSortedMap", Collections.unmodifiableSortedMap(new TreeMap<>(msrc)));

        System.out.println("== Collections.synchronized*");
        rt("synchronizedList", Collections.synchronizedList(src3));
        rt("synchronizedList(LinkedList)", Collections.synchronizedList(new LinkedList<>(src3)));
        rt("synchronizedSet", Collections.synchronizedSet(new LinkedHashSet<>(src3)));
        rt("synchronizedMap", Collections.synchronizedMap(msrc));

        System.out.println("== Collections singletons / empties");
        rt("emptyList", Collections.emptyList());
        rt("emptySet", Collections.emptySet());
        rt("emptyMap", Collections.emptyMap());
        rt("singletonList", Collections.singletonList("a"));
        rt("singleton", Collections.singleton("a"));
        rt("singletonMap", Collections.singletonMap("a", "1"));
        rt("nCopies", Collections.nCopies(3, "a"));

        System.out.println("== plain collections (control)");
        rt("ArrayList", new ArrayList<>(src3));
        rt("LinkedList", new LinkedList<>(src3));
        rt("HashSet", new HashSet<>(src3));
        rt("LinkedHashSet", new LinkedHashSet<>(src3));
        rt("TreeSet", new TreeSet<>(src3));
        rt("HashMap", new HashMap<>(msrc));
        rt("LinkedHashMap", new LinkedHashMap<>(msrc));
        rt("TreeMap", new TreeMap<>(msrc));
        rt("Hashtable", new Hashtable<>(msrc));
        rt("ArrayDeque->list", new ArrayList<>(new ArrayDeque<>(src3)));
        rt("Arrays.asList copy", new ArrayList<>(Arrays.asList("a", "b")));

        System.out.println("== nested (the Spring shape)");
        rt("List.of inside ArrayList", new ArrayList<Object>(List.of(Set.of("a"), Map.of("k", "v"), List.of("x"))));
        rt("unmodifiableList of List.of", Collections.unmodifiableList(new ArrayList<Object>(List.of("a", "b"))));

        System.out.println("TOTALFAILS=" + fails);
    }
}
