import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.Iterator;
import java.util.List;
import java.util.Set;
import java.util.concurrent.CopyOnWriteArraySet;

/**
 * The whole declared surface of `java.util.concurrent.CopyOnWriteArraySet`,
 * plus the four methods it inherits rather than declares.
 *
 * # Why this is its own file
 *
 * `probes/CollectionSlotFloor.java` drives this class through `setFamily`,
 * which is eight checks over `add`/`size`/`contains`/`remove`/`iterator`/
 * `clear`. That is enough to say a Set works and nowhere near enough to say
 * WHICH object is under it -- and the two possible answers have very different
 * costs. HotSpot backs a `CopyOnWriteArraySet` with a `CopyOnWriteArrayList`
 * (`private final CopyOnWriteArrayList<E> al`, the class's ONE instance field).
 * CratonVM backed it with a `LinkedHashMap`, which is 88 bytes empty against
 * the list's 48, and put that map in the slot the real class declares as `al`.
 *
 * Moving a class from a synthetic overlay onto its own bytecode is only safe if
 * the surface is covered first, because the failure mode is silence: a method
 * that reads the wrong backing answers "empty" rather than throwing. So this
 * file asserts every method `javap -p java.util.concurrent.CopyOnWriteArraySet`
 * lists, against values a wrong backing cannot produce.
 *
 * Insertion ORDER is asserted throughout and is not decoration: it is the
 * observable that separates a list-backed set from a hash-backed one, and the
 * JDK's own `CopyOnWriteArraySet` iterates in insertion order because a
 * `CopyOnWriteArrayList` does.
 *
 * Runs identically on HotSpot, which is the oracle. Exits non-zero on any
 * mismatch, so it is usable as a gate.
 */
public class CowSetBacking {
    static int failures = 0;

    public static void main(String[] args) {
        section("construction", CowSetBacking::construction);
        section("membership", CowSetBacking::membership);
        section("bulk", CowSetBacking::bulk);
        section("iteration", CowSetBacking::iteration);
        section("arrays", CowSetBacking::arrays);
        section("equality", CowSetBacking::equality);
        section("functional", CowSetBacking::functional);
        section("snapshot semantics", CowSetBacking::snapshot);
        section("empty reads", CowSetBacking::emptyReads);

        System.out.println(failures == 0 ? "PASS CowSetBacking"
                                         : "FAIL CowSetBacking (" + failures + ")");
        System.out.println("COWSET_END");
        if (failures != 0) System.exit(1);
    }

    /** A throw inside one section is a recorded failure, not the end of the run. */
    static void section(String name, Runnable body) {
        try {
            body.run();
        } catch (Throwable t) {
            failures++;
            System.out.println("  ERROR " + name + ": " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        }
    }

    static void check(String what, String expected, String actual) {
        if (!expected.equals(actual)) {
            failures++;
            System.out.println("  MISMATCH " + what + ": expected <" + expected
                    + "> got <" + actual + ">");
        }
    }

    static CopyOnWriteArraySet<String> of(String... elems) {
        CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
        for (String e : elems) s.add(e);
        return s;
    }

    /** Both constructors, and the duplicate-collapsing the collection form does. */
    static void construction() {
        CopyOnWriteArraySet<String> empty = new CopyOnWriteArraySet<>();
        check("new() size", "0", String.valueOf(empty.size()));
        check("new() isEmpty", "true", String.valueOf(empty.isEmpty()));

        // The collection constructor keeps FIRST-seen order and drops repeats.
        List<String> src = Arrays.asList("b", "a", "b", "c", "a");
        CopyOnWriteArraySet<String> fromColl = new CopyOnWriteArraySet<>(src);
        check("new(Collection) size", "3", String.valueOf(fromColl.size()));
        check("new(Collection) order", "[b, a, c]", fromColl.toString());

        // Copying a set of the same type goes down a different branch in the
        // JDK (it can take the source's array wholesale).
        CopyOnWriteArraySet<String> copy = new CopyOnWriteArraySet<>(fromColl);
        check("new(CopyOnWriteArraySet) size", "3", String.valueOf(copy.size()));
        check("new(CopyOnWriteArraySet) order", "[b, a, c]", copy.toString());
        copy.add("d");
        check("copy is independent", "3", String.valueOf(fromColl.size()));
    }

    /** `add` / `remove` / `contains` / `size` / `isEmpty` / `clear`. */
    static void membership() {
        CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
        check("add is true", "true", String.valueOf(s.add("a")));
        check("re-add is false", "false", String.valueOf(s.add("a")));
        check("size after dup", "1", String.valueOf(s.size()));

        for (int i = 0; i < 40; i++) s.add("e" + i);
        check("size after fill", "41", String.valueOf(s.size()));
        check("contains first", "true", String.valueOf(s.contains("a")));
        check("contains last", "true", String.valueOf(s.contains("e39")));
        // Equal-but-distinct key: the set must compare with `equals`, not `==`.
        check("contains by equals", "true", String.valueOf(s.contains("e" + 17)));
        check("contains absent", "false", String.valueOf(s.contains("nope")));

        check("remove present", "true", String.valueOf(s.remove("e0")));
        check("remove absent", "false", String.valueOf(s.remove("e0")));
        check("size after remove", "40", String.valueOf(s.size()));

        check("isEmpty while full", "false", String.valueOf(s.isEmpty()));
        s.clear();
        check("size after clear", "0", String.valueOf(s.size()));
        check("isEmpty after clear", "true", String.valueOf(s.isEmpty()));
        // A cleared set must still be usable.
        s.add("again");
        check("reuse after clear", "[again]", s.toString());
    }

    /** `addAll` / `removeAll` / `retainAll` / `containsAll`. */
    static void bulk() {
        CopyOnWriteArraySet<String> s = of("a", "b", "c");
        check("addAll new", "true", String.valueOf(s.addAll(Arrays.asList("d", "e"))));
        check("addAll order", "[a, b, c, d, e]", s.toString());
        check("addAll all-present is false", "false",
                String.valueOf(s.addAll(Arrays.asList("a", "d"))));
        check("addAll partial is true", "true",
                String.valueOf(s.addAll(Arrays.asList("a", "f"))));
        check("addAll partial order", "[a, b, c, d, e, f]", s.toString());
        check("addAll empty is false", "false",
                String.valueOf(s.addAll(new ArrayList<String>())));

        check("containsAll subset", "true",
                String.valueOf(s.containsAll(Arrays.asList("b", "e"))));
        check("containsAll superset", "false",
                String.valueOf(s.containsAll(Arrays.asList("b", "zz"))));
        check("containsAll empty", "true",
                String.valueOf(s.containsAll(new ArrayList<String>())));

        check("removeAll hits", "true", String.valueOf(s.removeAll(Arrays.asList("a", "f"))));
        check("removeAll order", "[b, c, d, e]", s.toString());
        check("removeAll misses", "false", String.valueOf(s.removeAll(Arrays.asList("zz"))));

        check("retainAll narrows", "true", String.valueOf(s.retainAll(Arrays.asList("c", "e"))));
        check("retainAll order", "[c, e]", s.toString());
        check("retainAll no-op", "false", String.valueOf(s.retainAll(Arrays.asList("c", "e"))));
    }

    /** `iterator` — order, exhaustion, and the copy-on-write refusal to remove. */
    static void iteration() {
        CopyOnWriteArraySet<String> s = of("x", "y", "z");
        StringBuilder sb = new StringBuilder();
        for (String e : s) sb.append(e).append(',');
        check("iteration order", "x,y,z,", sb.toString());

        Iterator<String> it = s.iterator();
        check("iterator hasNext", "true", String.valueOf(it.hasNext()));
        check("iterator next", "x", it.next());
        it.next();
        it.next();
        check("iterator exhausted", "false", String.valueOf(it.hasNext()));

        // A COW iterator is a SNAPSHOT and refuses `remove`.
        Iterator<String> snap = s.iterator();
        s.add("w");
        int seen = 0;
        while (snap.hasNext()) { snap.next(); seen++; }
        check("snapshot ignores later add", "3", String.valueOf(seen));
        try {
            Iterator<String> ro = s.iterator();
            ro.next();
            ro.remove();
            check("iterator.remove throws", "UnsupportedOperationException", "no throw");
        } catch (UnsupportedOperationException e) {
            check("iterator.remove throws", "UnsupportedOperationException",
                    "UnsupportedOperationException");
        }
    }

    /** `toArray()`, `toArray(T[])` in both the fits and the grows cases. */
    static void arrays() {
        CopyOnWriteArraySet<String> s = of("p", "q", "r");
        Object[] plain = s.toArray();
        check("toArray length", "3", String.valueOf(plain.length));
        check("toArray order", "[p, q, r]", Arrays.toString(plain));

        String[] exact = s.toArray(new String[3]);
        check("toArray(T[]) exact", "[p, q, r]", Arrays.toString(exact));

        String[] grown = s.toArray(new String[0]);
        check("toArray(T[]) grown", "[p, q, r]", Arrays.toString(grown));

        // Oversized destination: the JDK null-terminates at `size`.
        String[] big = s.toArray(new String[5]);
        check("toArray(T[]) oversized", "[p, q, r, null, null]", Arrays.toString(big));

        // `Collection.toArray(IntFunction)` — a default method, not declared by
        // the class, so it is a separate resolution path from the two above.
        String[] gen = s.toArray(String[]::new);
        check("toArray(IntFunction)", "[p, q, r]", Arrays.toString(gen));
    }

    /** `equals` (declared) and `hashCode` (inherited from AbstractSet). */
    static void equality() {
        CopyOnWriteArraySet<String> a = of("1", "2", "3");
        CopyOnWriteArraySet<String> b = of("3", "2", "1");
        check("equals ignores order", "true", String.valueOf(a.equals(b)));
        check("equals is symmetric", "true", String.valueOf(b.equals(a)));
        check("equals self", "true", String.valueOf(a.equals(a)));
        check("equals different size", "false", String.valueOf(a.equals(of("1", "2"))));
        check("equals non-set", "false", String.valueOf(a.equals(Arrays.asList("1", "2", "3"))));

        // Set.hashCode is the SUM of element hashes, so it is order-independent
        // and cross-implementation. A wrong backing shows up here as 0.
        Set<String> hs = new java.util.HashSet<>(Arrays.asList("1", "2", "3"));
        check("hashCode matches HashSet", String.valueOf(hs.hashCode()),
                String.valueOf(a.hashCode()));
        check("hashCode ignores order", String.valueOf(a.hashCode()),
                String.valueOf(b.hashCode()));
        check("empty hashCode", "0", String.valueOf(new CopyOnWriteArraySet<String>().hashCode()));

        // toString is AbstractCollection's, over the iterator.
        check("toString", "[1, 2, 3]", a.toString());
    }

    /** `forEach` / `removeIf` (declared), `stream` / `spliterator`. */
    static void functional() {
        CopyOnWriteArraySet<String> s = of("m", "n", "o");
        StringBuilder sb = new StringBuilder();
        s.forEach(e -> sb.append(e).append('.'));
        check("forEach order", "m.n.o.", sb.toString());

        check("stream count", "3", String.valueOf(s.stream().count()));
        check("stream collect", "[m, n, o]", s.stream().toList().toString());
        check("stream filter", "[n]",
                s.stream().filter(e -> e.equals("n")).toList().toString());
        check("spliterator estimate", "3",
                String.valueOf(s.spliterator().estimateSize()));

        check("removeIf hits", "true", String.valueOf(s.removeIf(e -> e.equals("n"))));
        check("removeIf order", "[m, o]", s.toString());
        check("removeIf misses", "false", String.valueOf(s.removeIf(e -> e.equals("zz"))));
    }

    /** The copy-on-write contract as a Collection sees it. */
    static void snapshot() {
        CopyOnWriteArraySet<String> s = of("a", "b");
        Collection<String> asColl = s;
        check("Collection.size", "2", String.valueOf(asColl.size()));
        check("Collection.contains", "true", String.valueOf(asColl.contains("b")));

        // Adding while holding an iterator must not raise CME — that is the
        // whole point of the class, and the failure it replaces is a
        // ConcurrentModificationException from a live cursor.
        Iterator<String> it = s.iterator();
        s.add("c");
        s.remove("a");
        int seen = 0;
        while (it.hasNext()) { it.next(); seen++; }
        check("no CME, snapshot size", "2", String.valueOf(seen));
        check("live set after mutation", "[b, c]", s.toString());
    }

    /** Reads against a set whose backing may never have been written. */
    static void emptyReads() {
        CopyOnWriteArraySet<String> s = new CopyOnWriteArraySet<>();
        check("empty size", "0", String.valueOf(s.size()));
        check("empty isEmpty", "true", String.valueOf(s.isEmpty()));
        check("empty contains", "false", String.valueOf(s.contains("k")));
        check("empty remove", "false", String.valueOf(s.remove("k")));
        check("empty iterator", "false", String.valueOf(s.iterator().hasNext()));
        check("empty toString", "[]", s.toString());
        check("empty toArray", "0", String.valueOf(s.toArray().length));
        check("empty toArray(T[])", "0", String.valueOf(s.toArray(new String[0]).length));
        check("empty stream", "0", String.valueOf(s.stream().count()));
        check("empty equals empty", "true",
                String.valueOf(s.equals(new CopyOnWriteArraySet<String>())));
        check("empty containsAll empty", "true",
                String.valueOf(s.containsAll(new ArrayList<String>())));
        check("empty removeAll", "false", String.valueOf(s.removeAll(Arrays.asList("k"))));
        check("empty retainAll", "false", String.valueOf(s.retainAll(Arrays.asList("k"))));
        check("empty removeIf", "false", String.valueOf(s.removeIf(e -> true)));
        s.forEach(e -> { throw new IllegalStateException("forEach on empty visited " + e); });
        s.clear();
        check("clear on empty then add", "[z]", add(s, "z").toString());
    }

    static Set<String> add(Set<String> s, String e) {
        s.add(e);
        return s;
    }
}
