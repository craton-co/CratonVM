import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;

/**
 * Where does the JDK give a HELPFUL NullPointerException, and where a bare one?
 *
 * `native-collections` funnels 85 call sites through three shared helpers --
 * `bare_npe`, `reject_null_functional`, `reject_null_collection` -- and all
 * three answer an NPE with a NULL message. That is correct exactly where the
 * JDK opens with `Objects.requireNonNull(x)`, and wrong everywhere it
 * DEREFERENCES the argument instead, because a dereference raises the VM's
 * helpful NPE naming the method and the JDK's own parameter name.
 *
 * `MapCtorMsgProbe` established that the map CONSTRUCTORS are the dereference
 * kind, and that one of them (`new HashMap<>(null)`) did not throw at all. This
 * asks the same question of the method surface those helpers guard.
 *
 * Every row prints the exception class and the message verbatim, so a null
 * message and a helpful one are distinguishable and the diff names the method.
 * Receivers are non-empty where the JDK's check order could otherwise be hidden
 * by an early size==0 return.
 */
public class NullArgMsgProbe {

    static int rows = 0;

    static void row(String what, Runnable r) {
        String cls = "no-throw", msg = "no-throw";
        try {
            r.run();
        } catch (Throwable t) {
            cls = t.getClass().getName();
            msg = String.valueOf(t.getMessage());
        }
        System.out.println((++rows) + " " + what + " |" + cls + "| msg |" + msg + "|");
    }

    static List<String> list() {
        return new ArrayList<>(List.of("a", "b", "c"));
    }

    static Map<String, String> map() {
        Map<String, String> m = new HashMap<>();
        m.put("a", "1");
        return m;
    }

    public static void main(String[] args) {
        // ---- Collection bulk operations. `AbstractCollection.addAll` etc. open
        // with `Objects.requireNonNull(c)` in some classes and dereference in
        // others; the receiver is non-empty so an early return cannot hide it.
        row("ArrayList.addAll(null)", () -> list().addAll(null));
        row("ArrayList.removeAll(null)", () -> list().removeAll(null));
        row("ArrayList.retainAll(null)", () -> list().retainAll(null));
        row("ArrayList.containsAll(null)", () -> list().containsAll(null));
        row("LinkedList.addAll(null)", () -> new LinkedList<>(list()).addAll(null));
        row("LinkedList.removeAll(null)", () -> new LinkedList<>(list()).removeAll(null));
        row("ArrayDeque.addAll(null)", () -> new ArrayDeque<>(list()).addAll(null));
        row("ArrayDeque.removeAll(null)", () -> new ArrayDeque<>(list()).removeAll(null));
        row("Vector.addAll(null)", () -> new Vector<>(list()).addAll(null));
        row("Vector.removeAll(null)", () -> new Vector<>(list()).removeAll(null));
        row("HashSet.addAll(null)", () -> new HashSet<>(list()).addAll(null));
        row("HashSet.retainAll(null)", () -> new HashSet<>(list()).retainAll(null));
        row("TreeSet.addAll(null)", () -> new TreeSet<>(list()).addAll(null));
        row("PriorityQueue.addAll(null)", () -> new PriorityQueue<>(list()).addAll(null));
        row("CopyOnWriteArrayList.addAll(null)",
            () -> new CopyOnWriteArrayList<>(list()).addAll(null));

        // ---- toArray(T[]) — a dereference of the array argument.
        row("ArrayList.toArray(null)", () -> list().toArray((String[]) null));
        row("HashSet.toArray(null)", () -> new HashSet<>(list()).toArray((String[]) null));
        row("ArrayDeque.toArray(null)", () -> new ArrayDeque<>(list()).toArray((String[]) null));

        // ---- Functional arguments. `forEach`/`removeIf`/`replaceAll` are
        // `Objects.requireNonNull` in the JDK; `compute*`/`merge` differ.
        row("ArrayList.forEach(null)", () -> list().forEach(null));
        row("ArrayList.removeIf(null)", () -> list().removeIf(null));
        row("ArrayList.replaceAll(null)", () -> list().replaceAll(null));
        row("ArrayList.sort(null)", () -> list().sort(null));
        row("HashMap.forEach(null)", () -> map().forEach(null));
        row("HashMap.replaceAll(null)", () -> map().replaceAll(null));
        row("HashMap.computeIfAbsent(k,null)", () -> map().computeIfAbsent("a", null));
        row("HashMap.computeIfPresent(k,null)", () -> map().computeIfPresent("a", null));
        row("HashMap.compute(k,null)", () -> map().compute("a", null));
        row("HashMap.merge(k,v,null)", () -> map().merge("a", "z", null));
        row("ConcurrentHashMap.forEach(null)",
            () -> { ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>(); m.put("a", "1"); m.forEach(null); });
        row("ConcurrentHashMap.computeIfAbsent(k,null)",
            () -> new ConcurrentHashMap<String, String>().computeIfAbsent("a", null));
        row("TreeMap.forEach(null)", () -> new TreeMap<>(map()).forEach(null));
        row("TreeMap.replaceAll(null)", () -> new TreeMap<>(map()).replaceAll(null));

        // ---- java.util.Collections statics: every one opens by dereferencing.
        row("Collections.sort(null)", () -> Collections.sort(null));
        row("Collections.reverse(null)", () -> Collections.reverse(null));
        row("Collections.shuffle(null)", () -> Collections.shuffle(null));
        row("Collections.unmodifiableList(null)", () -> Collections.unmodifiableList(null));
        row("Collections.unmodifiableMap(null)", () -> Collections.unmodifiableMap(null));
        row("Collections.synchronizedList(null)", () -> Collections.synchronizedList(null));
        row("Collections.max(null)", () -> Collections.max(null));
        row("Collections.min(null)", () -> Collections.min(null));
        row("Collections.addAll(null, \"a\")", () -> Collections.addAll(null, "a"));
        row("Collections.nCopies(2,null)", () -> Collections.nCopies(2, null).get(0));

        // ---- List.of / Map.of / Set.of: immutable factories reject null.
        row("List.of(null)", () -> List.of((String) null));
        row("Set.of(null)", () -> Set.of((String) null));
        row("Map.of(k,null)", () -> Map.of("a", (String) null));
        row("List.copyOf(null)", () -> List.copyOf(null));
        row("Set.copyOf(null)", () -> Set.copyOf(null));
        row("Map.copyOf(null)", () -> Map.copyOf(null));

        // ---- Map query surface with a null key on receivers that differ.
        row("Hashtable.get(null)", () -> new Hashtable<>(map()).get(null));
        row("Hashtable.containsKey(null)", () -> new Hashtable<>(map()).containsKey(null));
        row("ConcurrentHashMap.get(null)", () -> new ConcurrentHashMap<>(map()).get(null));
        row("TreeMap.get(null)", () -> new TreeMap<>(map()).get(null));
        row("HashMap.get(null)", () -> map().get(null));

        // ---- Iterator/stream adapters.
        row("List.iterator().forEachRemaining(null)",
            () -> list().iterator().forEachRemaining(null));
        row("Arrays.asList(null)", () -> Arrays.asList((Object[]) null));
        row("Arrays.sort(null)", () -> Arrays.sort((int[]) null));
        row("Objects.requireNonNull(null)", () -> Objects.requireNonNull(null));

        System.out.println("ROWS " + rows);
        System.out.println("DONE NullArgMsgProbe");
    }
}
