import java.util.*;
import java.util.concurrent.*;

/**
 * THE ITERATOR-CARRIER CENSUS.
 *
 * `native-collections` binds `native_al_itr_*` to a table of (collection class,
 * its real iterator class) pairs — `VALUES_ITR_CARRIERS` — so that a family
 * hands out the class HotSpot hands out, with the three snapshot fields past
 * its declared ones and the fail-fast door reading its `expectedModCount`.
 *
 * Two failure shapes live around that table, and this probe is what tells them
 * apart. Both are recorded in the source:
 *
 *   DORMANT ROW  a registration keyed on a class NOBODY MINTS. Harmless until
 *                someone mints one, then it wins the slot over the registration
 *                that matches the shape actually produced. `PriorityQueue$Itr`
 *                did exactly this on 2026-08-29 and iteration reported every
 *                queue EMPTY.
 *   TWO PRODUCERS  a carrier class java.base ALSO produces. `DescendingIterator
 *                extends DeqIterator`, so the real one arrives with four
 *                declared fields and none of the snapshot block, and the
 *                natives read a cursor out of the wrong slot —
 *                `descendingIterator()` walked to `[]`.
 *
 * Neither is visible from a registration list. Both are visible HERE: the class
 * name says which carrier a family produces, and the fail-fast column says
 * whether the natives bound to it are reading the shape it actually has.
 *
 * Diff against HotSpot. A `class=` difference is a family handing out the wrong
 * carrier; a `failFast=` difference on a matching class is the natives reading
 * it wrong.
 */
public final class ItrCarrierCensus {
    public static void main(String[] args) {
        // Sources in their own right.
        row("ArrayList", () -> new ArrayList<>(List.of("a", "b", "c")));
        row("LinkedList", () -> new LinkedList<>(List.of("a", "b", "c")));
        row("Vector", () -> vector());
        row("TreeSet", () -> new TreeSet<>(List.of("a", "b", "c")));
        row("HashSet", () -> new HashSet<>(List.of("a", "b", "c")));
        row("LinkedHashSet", () -> new LinkedHashSet<>(List.of("a", "b", "c")));
        row("ArrayDeque", () -> new ArrayDeque<>(List.of("a", "b", "c")));
        row("PriorityQueue", () -> new PriorityQueue<>(List.of("a", "b", "c")));
        row("CopyOnWriteArrayList", () -> new CopyOnWriteArrayList<>(List.of("a", "b", "c")));
        // Views — the original reason the carrier table exists. A view refuses
        // `add`, so its fail-fast has to be provoked by mutating the BACKING
        // MAP. Testing the view's own `add` would report `n/a-immutable` for
        // every one of them and measure nothing, which is what the first
        // version of this probe did.
        view("HashMap.keySet", Map::keySet, new HashMap<>());
        view("HashMap.values", Map::values, new HashMap<>());
        view("HashMap.entrySet", m -> m.entrySet(), new HashMap<>());
        view("TreeMap.keySet", Map::keySet, new TreeMap<>());
        view("TreeMap.values", Map::values, new TreeMap<>());
        view("TreeMap.entrySet", m -> m.entrySet(), new TreeMap<>());
        view("LinkedHashMap.keySet", Map::keySet, new LinkedHashMap<>());
        view("LinkedHashMap.values", Map::values, new LinkedHashMap<>());
        row("ArrayList.subList", () -> new ArrayList<>(List.of("a", "b", "c")).subList(0, 3));
        // Immutable / wrapper views: these must NOT be fail-fast, and saying so
        // is what stops "fail-fast everywhere" from reading as success.
        row("List.of", () -> List.of("a", "b", "c"));
        row("unmodifiableList", () -> Collections.unmodifiableList(new ArrayList<>(List.of("a", "b", "c"))));
        row("Arrays.asList", () -> Arrays.asList("a", "b", "c"));
        row("singletonList", () -> Collections.singletonList("a"));
        row("emptyList", () -> Collections.emptyList());
        // The second-producer probe: a REAL subclass of a carrier class.
        row("ArrayDeque.descending", () -> descending());
    }

    /** A map view: walk it, then provoke fail-fast by mutating the BACKING map. */
    static void view(String label,
                     java.util.function.Function<Map<String, String>, Collection<?>> pick,
                     Map<String, String> backing) {
        backing.put("a", "1");
        backing.put("b", "2");
        String cls, walk, ff;
        try {
            Collection<?> v = pick.apply(backing);
            cls = v.iterator().getClass().getName();
            StringBuilder sb = new StringBuilder();
            for (Object o : v) sb.append(o).append(',');
            walk = sb.toString();
            try {
                Iterator<?> it = v.iterator();
                it.next();
                backing.put("zzz", "9");
                it.next();
                ff = "false";
            } catch (ConcurrentModificationException e) {
                ff = "true";
            } catch (Throwable t) {
                ff = t.getClass().getSimpleName();
            }
        } catch (Throwable t) {
            System.out.println(label + " | ERROR " + t.getClass().getSimpleName() + ": " + t.getMessage());
            return;
        }
        System.out.println(label + " | class=" + cls + " walk=[" + walk + "] failFast=" + ff);
    }
    static List<String> vector() { Vector<String> v = new Vector<>(); v.add("a"); v.add("b"); v.add("c"); return v; }
    static Collection<String> descending() {
        ArrayDeque<String> d = new ArrayDeque<>(List.of("a", "b", "c"));
        List<String> out = new ArrayList<>();
        for (Iterator<String> it = d.descendingIterator(); it.hasNext(); ) out.add(it.next());
        // Returned so the row also prints what the descending walk produced.
        return out;
    }

    interface Src { Object make() throws Exception; }

    static void row(String label, Src src) {
        String cls, walk, ff;
        try {
            Object o = src.make();
            Iterator<?> it = (o instanceof Collection) ? ((Collection<?>) o).iterator()
                                                       : ((Iterable<?>) o).iterator();
            cls = it.getClass().getName();
            StringBuilder sb = new StringBuilder();
            for (Iterator<?> w = ((Collection<?>) o).iterator(); w.hasNext(); ) sb.append(w.next()).append(',');
            walk = sb.toString();
            ff = failFast(o);
        } catch (Throwable t) {
            System.out.println(label + " | ERROR " + t.getClass().getSimpleName() + ": " + t.getMessage());
            return;
        }
        System.out.println(label + " | class=" + cls + " walk=[" + walk + "] failFast=" + ff);
    }

    /** Structurally modify mid-iteration; HotSpot's fail-fast families raise CME. */
    @SuppressWarnings({"unchecked", "rawtypes"})
    static String failFast(Object o) {
        try {
            Collection c = (Collection) o;
            Iterator<?> it = c.iterator();
            it.next();
            try { c.add("zzz"); } catch (UnsupportedOperationException e) { return "n/a-immutable"; }
            it.next();
            return "false";
        } catch (ConcurrentModificationException e) {
            return "true";
        } catch (Throwable t) {
            return t.getClass().getSimpleName();
        }
    }
}
