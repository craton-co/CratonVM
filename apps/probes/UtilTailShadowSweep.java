import java.util.*;
import java.util.function.Consumer;

/** L3 tail, the last 20 rows: `java.util`'s two exception classes
 *  (`ConcurrentModificationException` 4, `NoSuchElementException` 4), the
 *  abstract and interface doors (`AbstractCollection` 3, `Collection` 2,
 *  `AbstractSet` 1, `Map` 1, `Spliterator` 1), the three entry classes
 *  (`HashMap$Node`, `Hashtable$Entry`, `TreeMap$Entry`) and the
 *  `ImmutableCollections` carriers behind `List.of` / `Set.of` / `Map.of`.
 *
 *  Four different kinds of edge, and none of them is a container operation:
 *
 *    * A JDK EXCEPTION'S FOUR CONSTRUCTORS. `NoSuchElementException(String)`
 *      leaves `getCause()` null and `NoSuchElementException(Throwable)` sets
 *      BOTH cause and message (the message is `cause.toString()`), which is the
 *      arity a from-memory implementation collapses;
 *    * AN ABSTRACT SUPERCLASS AS A DOOR. `AbstractCollection.toString`,
 *      `containsAll` and `toArray` are written entirely in terms of
 *      `iterator()` and `size()`, so a user class that supplies only those two
 *      gets the rest for free — and a native registered on the ABSTRACT class
 *      answers for that user class as well, whatever its own storage;
 *    * IMMUTABILITY THAT IS NOT A WRAPPER. `List.of` is not
 *      `unmodifiableList`: it REFUSES a null element up front, `Set.of` and
 *      `Map.of` refuse a DUPLICATE with `IllegalArgumentException`, and
 *      `List.of().contains(null)` is a NullPointerException rather than
 *      `false`;
 *    * `setValue` ON AN ENTRY, which three of the four entry classes support and
 *      `SimpleImmutableEntry` refuses.
 *
 *  DETERMINISM: `Set.of`/`Map.of` iteration order is deliberately randomised per
 *  JVM run by the JDK itself (SALT), so every one is sorted before printing and
 *  no `toString` of a multi-element immutable set or map appears anywhere.
 */
public class UtilTailShadowSweep {
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

    // ------------------------------------------------------------------
    // 1. the two exception classes, all four constructors each
    // ------------------------------------------------------------------
    static void exceptions() {
        ConcurrentModificationException c0 = new ConcurrentModificationException();
        p("CME() message", c0.getMessage());
        p("CME() cause", c0.getCause());
        p("CME() toString", c0.toString());
        ConcurrentModificationException c1 = new ConcurrentModificationException("m");
        p("CME(String) message", c1.getMessage());
        p("CME(String) cause", c1.getCause());
        p("CME(String) toString", c1.toString());
        Throwable boom = new IllegalStateException("boom");
        ConcurrentModificationException c2 = new ConcurrentModificationException(boom);
        // `(Throwable)` sets BOTH: the message is `cause.toString()`.
        p("CME(Throwable) message", c2.getMessage());
        p("CME(Throwable) cause is the same object", c2.getCause() == boom);
        ConcurrentModificationException c3 = new ConcurrentModificationException("m", boom);
        p("CME(String,Throwable) message", c3.getMessage());
        p("CME(String,Throwable) cause", c3.getCause() == boom);
        p("CME is a RuntimeException", c0 instanceof RuntimeException);
        p("CME null message ctor", new ConcurrentModificationException((String) null).getMessage());
        p("CME null cause ctor", new ConcurrentModificationException((Throwable) null).getMessage());

        NoSuchElementException n0 = new NoSuchElementException();
        p("NSEE() message", n0.getMessage());
        p("NSEE() toString", n0.toString());
        NoSuchElementException n1 = new NoSuchElementException("m");
        p("NSEE(String) message", n1.getMessage());
        p("NSEE(String) toString", n1.toString());
        NoSuchElementException n2 = new NoSuchElementException(boom);
        p("NSEE(Throwable) message", n2.getMessage());
        p("NSEE(Throwable) cause", n2.getCause() == boom);
        NoSuchElementException n3 = new NoSuchElementException("m", boom);
        p("NSEE(String,Throwable) message", n3.getMessage());
        p("NSEE(String,Throwable) cause", n3.getCause() == boom);
        p("NSEE is a RuntimeException", n0 instanceof RuntimeException);
        // The one an iterator actually raises, caught by type.
        t("empty iterator next is NSEE", () -> new ArrayList<String>().iterator().next());
        t("CME thrown and caught by type", () -> {
            try { throw new ConcurrentModificationException("x"); }
            catch (ConcurrentModificationException e) { throw new IllegalStateException("caught"); }
        });
    }

    // ------------------------------------------------------------------
    // 2. the abstract door: a user Collection with only iterator()+size()
    // ------------------------------------------------------------------
    static class TwoMethodCollection extends AbstractCollection<String> {
        private final List<String> backing = new ArrayList<>(Arrays.asList("a", "b", "c"));
        public Iterator<String> iterator() { return backing.iterator(); }
        public int size() { return backing.size(); }
    }
    static class TwoMethodSet extends AbstractSet<String> {
        private final List<String> backing = new ArrayList<>(Arrays.asList("a", "b"));
        public Iterator<String> iterator() { return backing.iterator(); }
        public int size() { return backing.size(); }
    }

    static void abstractDoors() {
        TwoMethodCollection c = new TwoMethodCollection();
        p("AbstractCollection toString", c.toString());
        p("AbstractCollection size", c.size());
        p("AbstractCollection isEmpty", c.isEmpty());
        p("AbstractCollection contains", c.contains("b"));
        p("AbstractCollection contains absent", c.contains("z"));
        p("AbstractCollection contains null", c.contains(null));
        p("AbstractCollection containsAll", c.containsAll(Arrays.asList("a", "c")));
        p("AbstractCollection containsAll false", c.containsAll(Arrays.asList("a", "z")));
        p("AbstractCollection toArray", Arrays.toString(c.toArray()));
        p("AbstractCollection toArray typed", Arrays.toString(c.toArray(new String[0])));
        p("AbstractCollection stream count", c.stream().count());
        StringBuilder sb = new StringBuilder();
        c.forEach(sb::append);
        p("AbstractCollection forEach", sb.toString());
        // `add` is UnsupportedOperationException unless overridden, and
        // `addAll` is written on top of it.
        t("AbstractCollection add", () -> c.add("z"));
        t("AbstractCollection addAll", () -> c.addAll(Arrays.asList("z")));
        p("AbstractCollection remove through the iterator", c.remove("a"));
        p("AbstractCollection after remove", c.toString());
        p("AbstractCollection removeAll", c.removeAll(Arrays.asList("b")));
        p("AbstractCollection after removeAll", c.toString());
        p("AbstractCollection retainAll", c.retainAll(new ArrayList<String>()));
        p("AbstractCollection after retainAll", c.toString());
        p("AbstractCollection clear leaves it empty", clearIt());

        TwoMethodSet s = new TwoMethodSet();
        p("AbstractSet toString", s.toString());
        // AbstractSet supplies equals/hashCode from the Set contract.
        p("AbstractSet equals a HashSet", s.equals(new HashSet<>(Arrays.asList("b", "a"))));
        p("AbstractSet hashCode agrees",
            s.hashCode() == new HashSet<>(Arrays.asList("b", "a")).hashCode());
        p("AbstractSet equals a List (must be false)", s.equals(Arrays.asList("a", "b")));

        // The INTERFACE doors, reached with an interface-typed reference.
        Collection<String> ic = new TwoMethodCollection();
        p("Collection door size", ic.size());
        p("Collection door contains", ic.contains("a"));
        p("Collection door toArray", Arrays.toString(ic.toArray()));
        Map<String, String> im = new HashMap<>();
        im.put("k", "v");
        p("Map door getOrDefault", im.getOrDefault("zz", "D"));
    }
    static String clearIt() {
        TwoMethodCollection c = new TwoMethodCollection();
        c.clear();
        return c.size() + "/" + c.toString();
    }

    // ------------------------------------------------------------------
    // 3. List.of / Set.of / Map.of — immutability that is not a wrapper
    // ------------------------------------------------------------------
    static void immutableFactories() {
        List<String> l0 = List.of();
        List<String> l2 = List.of("a", "b");
        List<String> ln = List.of("a", "b", "c", "d");
        p("List.of() size", l0.size());
        p("List.of(2) content", l2.toString());
        p("List.of(n) content", ln.toString());
        p("List.of get", ln.get(2));
        t("List.of get out of range", () -> ln.get(9));
        t("List.of add", () -> List.of("a").add("b"));
        t("List.of set", () -> List.of("a").set(0, "b"));
        t("List.of remove", () -> List.of("a").remove(0));
        t("List.of sort", () -> List.of("b", "a").sort(null));
        t("List.of iterator remove", () -> {
            Iterator<String> i = List.of("a").iterator(); i.next(); i.remove();
        });
        t("List.of with a null element", () -> List.of("a", null));
        // `contains(null)` is an NPE on an immutable list, not `false`.
        t("List.of contains null", () -> List.of("a").contains(null));
        t("List.of indexOf null", () -> List.of("a").indexOf(null));
        p("List.of contains", ln.contains("c"));
        p("List.of indexOf", ln.indexOf("c"));
        p("List.of equals an ArrayList", l2.equals(new ArrayList<>(Arrays.asList("a", "b"))));
        p("List.of hashCode agrees",
            l2.hashCode() == new ArrayList<>(Arrays.asList("a", "b")).hashCode());
        p("List.of subList", ln.subList(1, 3).toString());
        p("List.of toArray", Arrays.toString(l2.toArray()));
        p("List.copyOf", List.copyOf(new ArrayList<>(Arrays.asList("a", "b"))).toString());
        t("List.copyOf null", () -> List.copyOf(null));
        t("List.copyOf with a null element",
            () -> List.copyOf(new ArrayList<>(Arrays.asList("a", (String) null))));
        p("List.of stream count", ln.stream().count());
        p("List.of reversed", ln.reversed().toString());

        Set<String> s2 = Set.of("a", "b");
        p("Set.of size", s2.size());
        p("Set.of sorted content", sorted(s2));
        p("Set.of contains", s2.contains("a"));
        t("Set.of add", () -> Set.of("a").add("b"));
        t("Set.of remove", () -> Set.of("a").remove("a"));
        t("Set.of duplicate", () -> Set.of("a", "a"));
        t("Set.of with a null element", () -> Set.of("a", (String) null));
        t("Set.of contains null", () -> Set.of("a").contains(null));
        p("Set.of equals a HashSet", s2.equals(new HashSet<>(Arrays.asList("b", "a"))));
        p("Set.of hashCode agrees",
            s2.hashCode() == new HashSet<>(Arrays.asList("b", "a")).hashCode());
        p("Set.copyOf dedups", Set.copyOf(Arrays.asList("a", "a", "b")).size());
        t("Set.copyOf null", () -> Set.copyOf(null));

        Map<String, String> m1 = Map.of("k", "v");
        Map<String, String> m2 = Map.of("a", "1", "b", "2");
        p("Map.of size", m2.size());
        p("Map.of get", m2.get("b"));
        p("Map.of get absent", m2.get("zz"));
        p("Map.of sorted keys", sorted(m2.keySet()));
        p("Map.of sorted values", sorted(m2.values()));
        p("Map.of toString single", m1.toString());
        t("Map.of put", () -> Map.of("k", "v").put("a", "b"));
        t("Map.of remove", () -> Map.of("k", "v").remove("k"));
        t("Map.of duplicate key", () -> Map.of("a", "1", "a", "2"));
        t("Map.of null key", () -> Map.of((String) null, "1"));
        t("Map.of null value", () -> Map.of("a", (String) null));
        t("Map.of get null", () -> Map.of("a", "1").get(null));
        t("Map.of containsKey null", () -> Map.of("a", "1").containsKey(null));
        p("Map.of equals a HashMap", m1.equals(Collections.singletonMap("k", "v")));
        p("Map.of hashCode agrees",
            m1.hashCode() == Collections.singletonMap("k", "v").hashCode());
        p("Map.copyOf", Map.copyOf(Collections.singletonMap("k", "v")).get("k"));
        t("Map.copyOf null", () -> Map.copyOf(null));
        p("Map.ofEntries", Map.ofEntries(Map.entry("a", "1")).get("a"));
    }

    // ------------------------------------------------------------------
    // 4. the entry classes
    // ------------------------------------------------------------------
    static void entries() {
        Map.Entry<String, String> e = Map.entry("k", "v");
        p("Map.entry getKey", e.getKey());
        p("Map.entry getValue", e.getValue());
        p("Map.entry toString", e.toString());
        t("Map.entry setValue", () -> e.setValue("z"));
        t("Map.entry null key", () -> Map.entry(null, "v"));
        t("Map.entry null value", () -> Map.entry("k", null));

        AbstractMap.SimpleEntry<String, String> se = new AbstractMap.SimpleEntry<>("k", "v");
        p("SimpleEntry toString", se.toString());
        p("SimpleEntry setValue returns old", se.setValue("z"));
        p("SimpleEntry after setValue", se.getValue());
        p("SimpleEntry equals Map.entry", se.equals(Map.entry("k", "z")));
        p("SimpleEntry hashCode agrees", se.hashCode() == Map.entry("k", "z").hashCode());
        p("SimpleEntry allows a null value", new AbstractMap.SimpleEntry<>("k", null).getValue());

        AbstractMap.SimpleImmutableEntry<String, String> sie =
            new AbstractMap.SimpleImmutableEntry<>("k", "v");
        p("SimpleImmutableEntry toString", sie.toString());
        t("SimpleImmutableEntry setValue", () -> sie.setValue("z"));
        p("SimpleImmutableEntry equals SimpleEntry",
            sie.equals(new AbstractMap.SimpleEntry<>("k", "v")));

        // The live entry of each map family: setValue writes THROUGH.
        p("HashMap entry setValue", liveSetValue(new HashMap<>()));
        p("Hashtable entry setValue", liveSetValue(new Hashtable<>()));
        p("TreeMap entry setValue", liveSetValue(new TreeMap<>()));
        p("LinkedHashMap entry setValue", liveSetValue(new LinkedHashMap<>()));

        Comparator<Map.Entry<String, String>> byKey = Map.Entry.comparingByKey();
        Comparator<Map.Entry<String, String>> byValue = Map.Entry.comparingByValue();
        p("entry comparingByKey",
            byKey.compare(Map.entry("a", "1"), Map.entry("b", "2")) < 0);
        p("entry comparingByValue",
            byValue.compare(Map.entry("a", "2"), Map.entry("b", "1")) > 0);
    }
    static String liveSetValue(Map<String, String> m) {
        m.put("k", "v");
        for (Map.Entry<String, String> en : m.entrySet()) en.setValue("z");
        return m.get("k");
    }

    // ------------------------------------------------------------------
    // 5. Spliterator
    // ------------------------------------------------------------------
    static void spliterators() {
        List<String> l = new ArrayList<>(Arrays.asList("a", "b", "c", "d"));
        Spliterator<String> sp = l.spliterator();
        p("ArrayList spliterator estimateSize", sp.estimateSize());
        p("ArrayList spliterator SIZED", sp.hasCharacteristics(Spliterator.SIZED));
        p("ArrayList spliterator ORDERED", sp.hasCharacteristics(Spliterator.ORDERED));
        p("ArrayList spliterator SORTED", sp.hasCharacteristics(Spliterator.SORTED));
        p("ArrayList spliterator getExactSizeIfKnown", sp.getExactSizeIfKnown());
        StringBuilder one = new StringBuilder();
        p("tryAdvance", sp.tryAdvance(one::append));
        p("tryAdvance saw", one.toString());
        StringBuilder rest = new StringBuilder();
        sp.forEachRemaining((Consumer<String>) rest::append);
        p("forEachRemaining saw", rest.toString());
        p("tryAdvance when exhausted", sp.tryAdvance(x -> { }));

        Spliterator<String> sp2 = new ArrayList<>(Arrays.asList("a", "b", "c", "d")).spliterator();
        Spliterator<String> half = sp2.trySplit();
        p("trySplit is non-null", half != null);
        p("trySplit halves sum to the whole",
            (half == null ? 0 : half.estimateSize()) + sp2.estimateSize());
        Spliterator<String> single = new ArrayList<>(Arrays.asList("a")).spliterator();
        p("trySplit of one element", single.trySplit());

        Spliterator<String> ts = new TreeSet<>(Arrays.asList("b", "a")).spliterator();
        p("TreeSet spliterator SORTED", ts.hasCharacteristics(Spliterator.SORTED));
        p("TreeSet spliterator DISTINCT", ts.hasCharacteristics(Spliterator.DISTINCT));
        p("TreeSet spliterator comparator is null for natural", ts.getComparator());
        Spliterator<String> hs = new HashSet<>(Arrays.asList("a", "b")).spliterator();
        p("HashSet spliterator DISTINCT", hs.hasCharacteristics(Spliterator.DISTINCT));
        p("HashSet spliterator ORDERED", hs.hasCharacteristics(Spliterator.ORDERED));
        p("HashSet spliterator size", hs.estimateSize());
    }

    public static void main(String[] args) {
        exceptions();
        abstractDoors();
        immutableFactories();
        entries();
        spliterators();
        System.out.println("ROWS " + rows);
        System.out.println("DONE UtilTailShadowSweep");
    }
}
