import java.util.*;

/**
 * Behavioural cover for the per-class synthetic slot floors in
 * `class_manager.rs::synthetic_stub_fields`.
 *
 * Those floors exist for SYNTHETIC-JDK mode, where the stub class IS the object
 * layout and the collection natives write to synthetic slot indices that no real
 * class declares. A floor that is too LOW is silent: `set_field` drops an
 * out-of-range store rather than failing, so the write vanishes and the
 * collection quietly misbehaves later.
 *
 * That silence is the reason this probe exists as a separate artefact rather
 * than as a line in a design note. It asserts, for every collection family whose
 * floor is set in that table, that a store to each slot the natives use survives
 * a round trip. Run it in BOTH modes -- the synthetic-JDK arm needs a binary
 * built with `--features synthetic-jdk`, which is the arm `regression-suite`
 * does not cover.
 *
 * Exits non-zero if anything mismatched, so it is usable as a gate.
 *
 * KNOWN BASELINE, synthetic-JDK mode, MEASURED 2026-09-12 on `dev` at
 * `0ad29ab00`: SIXTEEN rows, and NONE of them is a slot floor. The count was
 * documented as eight and had not been re-measured since; sections added after
 * that reach further into the synthetic class library, and what they find there
 * is missing METHODS, not lost slots. Real-JDK mode is clean.
 *
 * <pre>
 *   8 MISMATCH  TreeMap.keySet/entrySet report 0; IdentityHashMap reports 0 for
 *               everything; LinkedHashSet and CopyOnWriteArraySet report 0 for
 *               size and after-remove   -- synthetic gaps in four families
 *   ERROR LinkedList            NoSuchMethodError LinkedList.indexOf
 *   ERROR ArrayDeque            NoSuchMethodError String.entrySet (iterator path)
 *   ERROR empty sorted reads    NPE: Set.iterator() returned null
 *   ERROR floor-exempt families NoSuchMethodError LinkedBlockingQueue$Itr.hasNext
 *   ERROR wrapped ArrayDeque    NoSuchMethodError String.entrySet (iterator path;
 *                               `toString` on the SAME deque answers `[x, y]`,
 *                               so the gap is the iterator, not the state)
 *   ERROR Properties chain names  UnsupportedOperationException from
 *                               stringPropertyNames
 * </pre>
 *
 * An unchanged build produces the IDENTICAL sixteen. Compare the SET against
 * that baseline rather than expecting a bare pass in synthetic mode, and when a
 * section starts throwing there, give it its own `section(..)` so it stops
 * hiding the checks behind it rather than deleting the check.
 *
 * See docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md
 */
public class CollectionSlotFloor {
    static int failures = 0;

    public static void main(String[] args) {
        // Maps: buckets / size / capacity, plus growth past the initial table.
        section("HashMap", () -> mapFamily("HashMap", new HashMap<String, String>()));
        section("Hashtable", () -> mapFamily("Hashtable", new Hashtable<String, String>()));
        section("LinkedHashMap", () -> mapFamily("LinkedHashMap", new LinkedHashMap<String, String>()));
        section("ConcurrentHashMap",
                () -> mapFamily("ConcurrentHashMap",
                        new java.util.concurrent.ConcurrentHashMap<String, String>()));
        section("TreeMap", () -> mapFamily("TreeMap", new TreeMap<String, String>()));
        // NOT through `mapFamily`: IdentityHashMap keys on reference identity, so
        // a lookup with an equal-but-distinct String is a legitimate miss there.
        // Its slots get their own check below.
        section("IdentityHashMap", CollectionSlotFloor::identityMap);

        section("HashSet", () -> setFamily("HashSet", new HashSet<String>()));
        section("LinkedHashSet", () -> setFamily("LinkedHashSet", new LinkedHashSet<String>()));
        section("TreeSet", () -> setFamily("TreeSet", new TreeSet<String>()));

        section("ArrayList", () -> listFamily("ArrayList", new ArrayList<String>()));
        section("Vector", () -> listFamily("Vector", new Vector<String>()));
        section("LinkedList", () -> listFamily("LinkedList", new LinkedList<String>()));
        section("ArrayDeque", () -> listFamily("ArrayDeque", new ArrayDeque<String>()));
        section("CopyOnWriteArrayList",
                () -> listFamily("CopyOnWriteArrayList",
                        new java.util.concurrent.CopyOnWriteArrayList<String>()));

        // LinkedHashMap's head/tail slots (3 and 4) are what make it ORDERED.
        // A lost write there reads as a HashMap with the right contents and the
        // wrong iteration order -- the quietest failure in this whole file.
        section("LinkedHashMap order", () -> {
            LinkedHashMap<String, Integer> ordered = new LinkedHashMap<>();
            for (int i = 0; i < 12; i++) ordered.put("k" + i, i);
            StringBuilder sb = new StringBuilder();
            for (Map.Entry<String, Integer> e : ordered.entrySet()) {
                sb.append(e.getValue()).append(',');
            }
            check("LinkedHashMap insertion order", "0,1,2,3,4,5,6,7,8,9,10,11,", sb.toString());
        });

        // The EMPTY read path, for the sorted families.
        //
        // Every section above fills its collection first, so none of them ever
        // reads one that has no backing array. `TreeMap`/`TreeSet` install
        // theirs on the first insert (`native_tm_put` / `native_ts_add`), which
        // makes "never written" a state the readers have to handle rather than
        // a state they happen never to see -- and a reader that assumes an
        // array reads a fresh map as a crash or as garbage, not as empty.
        section("empty sorted reads", CollectionSlotFloor::emptySortedReads);
        section("empty concurrent reads", CollectionSlotFloor::emptyConcurrentReads);

        // The classes whose floor is SYNTHETIC-ONLY.
        //
        // `FLOOR_EXEMPT_CLASSES` (`classloading/src/class_manager.rs`) stops
        // padding these to their fabricated slot count when they are defined
        // from real class-file bytes, because their real layouts are narrower
        // and padding cost them the compact layout for every slot. Dropping a
        // floor fails SILENTLY -- an out-of-range `set_field` is dropped, not
        // raised -- so this section drives each of them through the surface a
        // lost write would take out, in BOTH modes: a native that still wrote
        // a raw absolute slot past the real width would read back as an empty
        // or a garbled collection here, not as an error anywhere.
        section("floor-exempt families", CollectionSlotFloor::floorExemptFamilies);

        // Four states the section above does not reach, on the same seven
        // classes. Each is a reader a narrowed floor could break WITHOUT
        // breaking anything asserted there, which is the only reason to add a
        // check to a family already covered. One section each: the last two
        // throw in synthetic-JDK mode for reasons that are not floors, and a
        // shared section would let either of them hide the rest.
        section("LinkedHashSet order", CollectionSlotFloor::linkedHashSetOrder);
        section("empty exempt sets", CollectionSlotFloor::emptyExemptSets);
        section("wrapped ArrayDeque", CollectionSlotFloor::wrappedArrayDeque);
        section("Properties chain names",
                CollectionSlotFloor::propertiesDefaultsChainNames);

        // EnumMap / EnumSet have their own floors.
        section("EnumMap/EnumSet", () -> {
            EnumMap<Day, String> em = new EnumMap<>(Day.class);
            em.put(Day.WED, "w");
            em.put(Day.MON, "m");
            check("EnumMap size", "2", String.valueOf(em.size()));
            check("EnumMap get", "w", String.valueOf(em.get(Day.WED)));
            check("EnumMap key order", "[MON, WED]", em.keySet().toString());
            EnumSet<Day> es = EnumSet.of(Day.FRI, Day.MON);
            check("EnumSet size", "2", String.valueOf(es.size()));
            check("EnumSet contains", "true", String.valueOf(es.contains(Day.FRI)));
        });

        section("PriorityQueue", () -> {
            PriorityQueue<Integer> pq = new PriorityQueue<>(List.of(5, 1, 4, 2, 3));
            check("PriorityQueue head", "1", String.valueOf(pq.peek()));
            check("PriorityQueue size", "5", String.valueOf(pq.size()));
        });

        section("StringJoiner", () -> {
            StringJoiner sj = new StringJoiner(",", "[", "]");
            sj.add("a").add("b");
            check("StringJoiner", "[a,b]", sj.toString());
        });

        System.out.println(failures == 0 ? "PASS CollectionSlotFloor"
                                         : "FAIL CollectionSlotFloor (" + failures + ")");
        System.out.println("SLOTFLOOR_END");
        if (failures != 0) System.exit(1);
    }

    /**
     * Run one section, and turn a THROW inside it into a recorded failure
     * rather than the end of the run.
     *
     * Not defensiveness: the synthetic-JDK arm is the one that matters when a
     * floor moves, and in that arm a class library gap several sections in
     * used to take every section after it with it. `LinkedList.indexOf` is the
     * standing example -- absent from the synthetic image, thrown at section
     * six of fourteen, so the eight sections after it (including every
     * floor-exempt family this file exists to cover) were never reached and
     * their silence read as agreement. An ERROR row counts as a failure, so a
     * section that starts throwing is still visible; it just no longer hides
     * the ones behind it.
     */
    static void section(String name, Runnable body) {
        try {
            body.run();
        } catch (Throwable t) {
            failures++;
            System.out.println("  ERROR " + name + ": " + t.getClass().getName()
                    + (t.getMessage() == null ? "" : ": " + t.getMessage()));
        }
    }

    enum Day { MON, WED, FRI }

    /** `IdentityHashMap` keyed by identity, so every key is held. */
    static void identityMap() {
        IdentityHashMap<String, String> m = new IdentityHashMap<>();
        String[] keys = new String[40];
        for (int i = 0; i < 40; i++) {
            // Runtime concatenation, not `new String(String)`: the copy
            // constructor is absent from the synthetic class library, and this
            // probe has to run in that mode -- that is the whole point of it.
            // Concatenation still yields a fresh, un-interned instance.
            keys[i] = "k" + i;
            m.put(keys[i], "v" + i);
        }
        check("IdentityHashMap size", "40", String.valueOf(m.size()));
        check("IdentityHashMap get first", "v0", String.valueOf(m.get(keys[0])));
        check("IdentityHashMap get last", "v39", String.valueOf(m.get(keys[39])));
        check("IdentityHashMap identity miss", "null", String.valueOf(m.get("k" + 0)));
        m.remove(keys[0]);
        check("IdentityHashMap after remove", "39", String.valueOf(m.size()));
        m.clear();
        check("IdentityHashMap after clear", "true", String.valueOf(m.isEmpty()));
    }

    /** Grows past the initial table so the resize path's slot writes are covered too. */
    static void mapFamily(String name, Map<String, String> m) {
        for (int i = 0; i < 40; i++) m.put("k" + i, "v" + i);
        check(name + " size", "40", String.valueOf(m.size()));
        check(name + " get first", "v0", String.valueOf(m.get("k0")));
        check(name + " get last", "v39", String.valueOf(m.get("k39")));
        check(name + " containsKey", "true", String.valueOf(m.containsKey("k17")));
        check(name + " keySet size", "40", String.valueOf(m.keySet().size()));
        check(name + " values size", "40", String.valueOf(m.values().size()));
        check(name + " entrySet size", "40", String.valueOf(m.entrySet().size()));
        m.remove("k0");
        check(name + " after remove", "39", String.valueOf(m.size()));
        check(name + " removed is gone", "null", String.valueOf(m.get("k0")));
        m.clear();
        check(name + " after clear", "true", String.valueOf(m.isEmpty()));
    }

    static void setFamily(String name, Set<String> s) {
        for (int i = 0; i < 40; i++) s.add("e" + i);
        check(name + " size", "40", String.valueOf(s.size()));
        check(name + " contains", "true", String.valueOf(s.contains("e39")));
        check(name + " re-add is false", "false", String.valueOf(s.add("e39")));
        s.remove("e0");
        check(name + " after remove", "39", String.valueOf(s.size()));
        int seen = 0;
        for (String ignored : s) seen++;
        check(name + " iterated", "39", String.valueOf(seen));
        s.clear();
        check(name + " after clear", "true", String.valueOf(s.isEmpty()));
    }

    static void listFamily(String name, Collection<String> c) {
        for (int i = 0; i < 40; i++) c.add("x" + i);
        check(name + " size", "40", String.valueOf(c.size()));
        check(name + " contains", "true", String.valueOf(c.contains("x39")));
        int seen = 0;
        for (String ignored : c) seen++;
        check(name + " iterated", "40", String.valueOf(seen));
        if (c instanceof List<?> l) {
            check(name + " get(0)", "x0", String.valueOf(l.get(0)));
            check(name + " get(39)", "x39", String.valueOf(l.get(39)));
            check(name + " indexOf", "17", String.valueOf(l.indexOf("x17")));
        }
        c.remove("x0");
        check(name + " after remove", "39", String.valueOf(c.size()));
        check(name + " toArray length", "39", String.valueOf(c.toArray().length));
        c.clear();
        check(name + " after clear", "true", String.valueOf(c.isEmpty()));
    }

    /// Reads against a sorted collection that has never been written.
    static void emptySortedReads() {
        TreeMap<String, String> tm = new TreeMap<>();
        check("empty TreeMap size", "0", String.valueOf(tm.size()));
        check("empty TreeMap isEmpty", "true", String.valueOf(tm.isEmpty()));
        check("empty TreeMap get", "null", String.valueOf(tm.get("k")));
        check("empty TreeMap containsKey", "false", String.valueOf(tm.containsKey("k")));
        check("empty TreeMap remove", "null", String.valueOf(tm.remove("k")));
        check("empty TreeMap keySet", "0", String.valueOf(tm.keySet().size()));
        check("empty TreeMap entrySet", "0", String.valueOf(tm.entrySet().size()));
        check("empty TreeMap values", "0", String.valueOf(tm.values().size()));
        check("empty TreeMap iterator", "false",
                String.valueOf(tm.entrySet().iterator().hasNext()));
        check("empty TreeMap toString", "{}", tm.toString());
        check("empty TreeMap equals empty", "true",
                String.valueOf(tm.equals(new TreeMap<String, String>())));
        try {
            tm.firstKey();
            check("empty TreeMap firstKey throws", "NoSuchElementException", "no throw");
        } catch (NoSuchElementException e) {
            check("empty TreeMap firstKey throws", "NoSuchElementException",
                    "NoSuchElementException");
        }
        tm.clear();
        check("empty TreeMap clear then put/get", "v", put1(tm));

        TreeMap<String, String> cmp = new TreeMap<>(Comparator.reverseOrder());
        check("empty TreeMap(cmp) size", "0", String.valueOf(cmp.size()));
        check("empty TreeMap(cmp) get", "null", String.valueOf(cmp.get("k")));
        cmp.put("a", "1");
        cmp.put("b", "2");
        check("empty TreeMap(cmp) keeps comparator", "b", cmp.firstKey());

        TreeSet<String> ts = new TreeSet<>();
        check("empty TreeSet size", "0", String.valueOf(ts.size()));
        check("empty TreeSet isEmpty", "true", String.valueOf(ts.isEmpty()));
        check("empty TreeSet contains", "false", String.valueOf(ts.contains("e")));
        check("empty TreeSet remove", "false", String.valueOf(ts.remove("e")));
        check("empty TreeSet iterator", "false", String.valueOf(ts.iterator().hasNext()));
        check("empty TreeSet toString", "[]", ts.toString());
        check("empty TreeSet toArray length", "0", String.valueOf(ts.toArray().length));
        check("empty TreeSet equals empty", "true",
                String.valueOf(ts.equals(new TreeSet<String>())));
        try {
            ts.first();
            check("empty TreeSet first throws", "NoSuchElementException", "no throw");
        } catch (NoSuchElementException e) {
            check("empty TreeSet first throws", "NoSuchElementException",
                    "NoSuchElementException");
        }
        ts.clear();
        ts.add("z");
        ts.add("a");
        check("empty TreeSet clear then add", "a", ts.first());

        TreeSet<String> tsc = new TreeSet<>(Comparator.reverseOrder());
        check("empty TreeSet(cmp) size", "0", String.valueOf(tsc.size()));
        tsc.add("a");
        tsc.add("b");
        check("empty TreeSet(cmp) keeps comparator", "b", tsc.first());

        // The NAVIGABLE readers, which derive a view from the backing store
        // rather than reading an element out of it -- the ones most likely to
        // assume the store exists.
        TreeMap<String, String> nav = new TreeMap<>();
        check("empty TreeMap firstEntry", "null", String.valueOf(nav.firstEntry()));
        check("empty TreeMap lastEntry", "null", String.valueOf(nav.lastEntry()));
        check("empty TreeMap ceilingKey", "null", String.valueOf(nav.ceilingKey("k")));
        check("empty TreeMap floorKey", "null", String.valueOf(nav.floorKey("k")));
        check("empty TreeMap higherKey", "null", String.valueOf(nav.higherKey("k")));
        check("empty TreeMap lowerKey", "null", String.valueOf(nav.lowerKey("k")));
        check("empty TreeMap pollFirstEntry", "null", String.valueOf(nav.pollFirstEntry()));
        check("empty TreeMap headMap", "0", String.valueOf(nav.headMap("m").size()));
        check("empty TreeMap tailMap", "0", String.valueOf(nav.tailMap("m").size()));
        check("empty TreeMap subMap", "0", String.valueOf(nav.subMap("a", "z").size()));
        check("empty TreeMap descendingMap", "0", String.valueOf(nav.descendingMap().size()));
        check("empty TreeMap descendingKeySet", "0",
                String.valueOf(nav.descendingKeySet().size()));
        check("empty TreeMap copy ctor", "0",
                String.valueOf(new TreeMap<String, String>(nav).size()));
        check("empty TreeMap putAll of empty", "0", putAllEmpty(nav));

        TreeSet<String> nts = new TreeSet<>();
        check("empty TreeSet pollFirst", "null", String.valueOf(nts.pollFirst()));
        check("empty TreeSet pollLast", "null", String.valueOf(nts.pollLast()));
        check("empty TreeSet ceiling", "null", String.valueOf(nts.ceiling("k")));
        check("empty TreeSet floor", "null", String.valueOf(nts.floor("k")));
        check("empty TreeSet headSet", "0", String.valueOf(nts.headSet("m").size()));
        check("empty TreeSet tailSet", "0", String.valueOf(nts.tailSet("m").size()));
        check("empty TreeSet subSet", "0", String.valueOf(nts.subSet("a", "z").size()));
        check("empty TreeSet descendingSet", "0", String.valueOf(nts.descendingSet().size()));
        check("empty TreeSet copy ctor", "0", String.valueOf(new TreeSet<String>(nts).size()));
        check("empty TreeSet addAll of empty", "false",
                String.valueOf(nts.addAll(new TreeSet<String>())));
    }

    static String putAllEmpty(TreeMap<String, String> m) {
        m.putAll(new TreeMap<String, String>());
        return String.valueOf(m.size());
    }

    static String put1(TreeMap<String, String> m) {
        m.put("k", "v");
        return String.valueOf(m.get("k"));
    }

    /// Reads and inserting operations against a `ConcurrentHashMap` whose
    /// segments have never been allocated.
    ///
    /// Its default constructor installs them on the first INSERT
    /// (`chm_segment_for_mut`) rather than eagerly, so "no segments yet" is a
    /// state every reader has to handle and every inserting entry point has to
    /// resolve. The read-only paths must report an empty map; the inserting
    /// ones -- put, putIfAbsent, merge, compute, computeIfAbsent, replace --
    /// must each install the segments themselves, and a path that reached its
    /// insert through the read-only lookup would silently store NOTHING and
    /// report success.
    /**
     * The six classes exempted from the synthetic slot floor in real-JDK mode,
     * driven through the reads and writes that a lost raw-slot write removes.
     *
     * Each is filled, read back every way the class offers, mutated, and
     * emptied. `Properties` additionally exercises the `defaults` chain, which
     * is the one slot on that class a native resolves BY NAME with the
     * fabricated model index as its fallback -- so it is where a wrong slot
     * would show up first.
     */
    /// Insertion ORDER on a `LinkedHashSet`, which nothing else here reads.
    ///
    /// Its floor went 3 -> 1 with `FLOOR_EXEMPT_CLASSES`, and order is the one
    /// thing a `LinkedHashSet` has that a `HashSet` does not. It is also the
    /// quietest thing to lose: right contents, right size, wrong sequence, no
    /// exception anywhere. `setFamily` cannot see it.
    static void linkedHashSetOrder() {
        LinkedHashSet<String> lhs = new LinkedHashSet<>();
        for (int i = 0; i < 12; i++) lhs.add("e" + i);
        StringBuilder order = new StringBuilder();
        for (String s : lhs) order.append(s).append(',');
        check("LinkedHashSet insertion order",
                "e0,e1,e2,e3,e4,e5,e6,e7,e8,e9,e10,e11,", order.toString());
        check("LinkedHashSet re-add is false", "false", String.valueOf(lhs.add("e0")));
        lhs.remove("e5");
        StringBuilder afterRemove = new StringBuilder();
        for (String s : lhs) afterRemove.append(s).append(',');
        check("LinkedHashSet order after remove",
                "e0,e1,e2,e3,e4,e6,e7,e8,e9,e10,e11,", afterRemove.toString());
    }

    /// The never-written receiver, for the three exempt SET classes.
    ///
    /// `setFamily` fills before it reads, so "empty" is a state its readers are
    /// never handed — and an empty set is exactly where a reader that assumes a
    /// backing store exists answers with a crash or with garbage instead of
    /// with nothing.
    static void emptyExemptSets() {
        HashSet<String> emptySet = new HashSet<>();
        check("empty HashSet size", "0", String.valueOf(emptySet.size()));
        check("empty HashSet contains", "false", String.valueOf(emptySet.contains("a")));
        check("empty HashSet iterator", "false",
                String.valueOf(emptySet.iterator().hasNext()));
        check("empty HashSet toString", "[]", emptySet.toString());
        check("empty HashSet remove", "false", String.valueOf(emptySet.remove("a")));
        check("empty HashSet toArray", "0", String.valueOf(emptySet.toArray().length));

        LinkedHashSet<String> emptyLhs = new LinkedHashSet<>();
        check("empty LinkedHashSet size", "0", String.valueOf(emptyLhs.size()));
        check("empty LinkedHashSet iterator", "false",
                String.valueOf(emptyLhs.iterator().hasNext()));

        java.util.concurrent.CopyOnWriteArraySet<String> emptyCows =
                new java.util.concurrent.CopyOnWriteArraySet<>();
        check("empty COWArraySet size", "0", String.valueOf(emptyCows.size()));
        check("empty COWArraySet isEmpty", "true", String.valueOf(emptyCows.isEmpty()));
        check("empty COWArraySet iterator", "false",
                String.valueOf(emptyCows.iterator().hasNext()));

        ArrayDeque<String> emptyDq = new ArrayDeque<>();
        check("empty ArrayDeque size", "0", String.valueOf(emptyDq.size()));
        check("empty ArrayDeque poll", "null", String.valueOf(emptyDq.poll()));
        check("empty ArrayDeque peek", "null", String.valueOf(emptyDq.peek()));
        check("empty ArrayDeque toString", "[]", emptyDq.toString());
        check("empty ArrayDeque toArray", "0", String.valueOf(emptyDq.toArray().length));
    }

    /// The SMALL wrapped deque, which is a different check from the large one.
    ///
    /// `collection_elements` tests the receiver's WIDTH to decide it
    /// understands an `ArrayDeque`, then reads slots 0..=2 and derives the
    /// count. With the fourth slot gone, a width test that still demanded four
    /// drops every real deque into the generic `f0 = array, f1 = size`
    /// heuristic below it — which reads `head` as the count. Two `addFirst` on
    /// a fresh deque leave `head = 15`, so that path yields fifteen nulls where
    /// this asserts two elements. That is the 2026-08-11 `stream()` defect, and
    /// a 60-element deque does NOT reproduce it: filling forward leaves
    /// `head = 0`, which makes the wrong reading look right.
    ///
    /// SYNTHETIC-JDK: iterating this receiver throws there (see the class
    /// comment's baseline). `toString` on the same deque answers `[x, y]`, so
    /// the gap is in the iterator path alone; it is a synthetic-mode gap and
    /// not a floor one, which is why it sits in its own section rather than
    /// taking the checks around it down with it.
    static void wrappedArrayDeque() {
        ArrayDeque<String> wrapped = new ArrayDeque<>();
        wrapped.addFirst("y");
        wrapped.addFirst("x");
        check("ArrayDeque wrapped size", "2", String.valueOf(wrapped.size()));
        check("ArrayDeque wrapped toString", "[x, y]", wrapped.toString());
        check("ArrayDeque wrapped toArray", "2", String.valueOf(wrapped.toArray().length));
        StringBuilder wrappedOrder = new StringBuilder();
        for (String s : wrapped) wrappedOrder.append(s);
        check("ArrayDeque wrapped iteration", "xy", wrappedOrder.toString());
    }

    /// `stringPropertyNames`, which walks the `defaults` chain to build a SET.
    ///
    /// A different reader from `getProperty`, which walks it to answer one key,
    /// and from `size()`, which deliberately does not walk it at all. The link
    /// it follows is the one that used to live in a raw model slot — absolute
    /// 3, which on a real `Properties` is `Hashtable.loadFactor`, a float.
    ///
    /// SYNTHETIC-JDK: throws `UnsupportedOperationException` there (see the
    /// class comment's baseline). Its own section for the same reason as
    /// `wrappedArrayDeque`.
    static void propertiesDefaultsChainNames() {
        Properties chainBase = new Properties();
        chainBase.setProperty("shared", "from-defaults");
        chainBase.setProperty("only-in-base", "b");
        Properties chainTop = new Properties(chainBase);
        chainTop.setProperty("shared", "from-derived");
        chainTop.setProperty("only-in-derived", "d");
        Set<String> names = chainTop.stringPropertyNames();
        check("Properties stringPropertyNames spans the chain", "3",
                String.valueOf(names.size()));
        check("Properties stringPropertyNames sees the inherited key", "true",
                String.valueOf(names.contains("only-in-base")));
        check("Properties stringPropertyNames does not duplicate the override", "true",
                String.valueOf(names.contains("shared")));
    }

    static void floorExemptFamilies() {
        setFamily("CopyOnWriteArraySet", new java.util.concurrent.CopyOnWriteArraySet<String>());

        listFamily("ConcurrentLinkedQueue", new java.util.concurrent.ConcurrentLinkedQueue<String>());
        listFamily("ConcurrentLinkedDeque", new java.util.concurrent.ConcurrentLinkedDeque<String>());

        // Queue/Deque ORDER, which `size`/`contains` cannot see. The four-slot
        // fabricated model keeps a ring buffer, a head index, a count and a
        // capacity; the real classes keep two `Node` references. A receiver
        // carrying one shape and read through the other answers in FIFO order
        // for a while and then stops.
        java.util.concurrent.ConcurrentLinkedQueue<String> clq =
                new java.util.concurrent.ConcurrentLinkedQueue<>();
        for (int i = 0; i < 24; i++) clq.offer("q" + i);
        check("CLQ peek", "q0", String.valueOf(clq.peek()));
        StringBuilder clqOrder = new StringBuilder();
        for (int i = 0; i < 24; i++) clqOrder.append(clq.poll()).append(',');
        StringBuilder clqWant = new StringBuilder();
        for (int i = 0; i < 24; i++) clqWant.append("q").append(i).append(',');
        check("CLQ FIFO order", clqWant.toString(), clqOrder.toString());
        check("CLQ drained", "true", String.valueOf(clq.isEmpty()));
        check("CLQ poll when empty", "null", String.valueOf(clq.poll()));

        java.util.concurrent.ConcurrentLinkedDeque<String> cld =
                new java.util.concurrent.ConcurrentLinkedDeque<>();
        for (int i = 0; i < 12; i++) cld.addLast("l" + i);
        for (int i = 0; i < 12; i++) cld.addFirst("f" + i);
        check("CLD size", "24", String.valueOf(cld.size()));
        check("CLD peekFirst", "f11", String.valueOf(cld.peekFirst()));
        check("CLD peekLast", "l11", String.valueOf(cld.peekLast()));
        check("CLD pollFirst", "f11", String.valueOf(cld.pollFirst()));
        check("CLD pollLast", "l11", String.valueOf(cld.pollLast()));
        check("CLD after polls", "22", String.valueOf(cld.size()));

        // ArrayDeque again, but through the DEQUE surface rather than as a
        // plain Collection. Its fourth slot -- a count the natives used to
        // keep beside the real `elements`/`head`/`tail` -- is gone, and the
        // count is derived from `head`/`tail` the way the JDK derives it. A
        // deque that wraps its ring buffer is where a derived count and a
        // stored one disagree, so fill past the initial capacity.
        ArrayDeque<String> dq = new ArrayDeque<>();
        for (int i = 0; i < 40; i++) dq.addLast("d" + i);
        for (int i = 0; i < 20; i++) dq.addFirst("h" + i);
        check("ArrayDeque size after wrap", "60", String.valueOf(dq.size()));
        check("ArrayDeque peekFirst", "h19", String.valueOf(dq.peekFirst()));
        check("ArrayDeque peekLast", "d39", String.valueOf(dq.peekLast()));
        int drained = 0;
        while (dq.pollFirst() != null) drained++;
        check("ArrayDeque drained count", "60", String.valueOf(drained));
        check("ArrayDeque empty after drain", "true", String.valueOf(dq.isEmpty()));

        // Properties: the map surface, and then the `defaults` chain.
        Properties base = new Properties();
        base.setProperty("shared", "from-defaults");
        base.setProperty("only-in-base", "b");
        Properties derived = new Properties(base);
        derived.setProperty("shared", "from-derived");
        derived.setProperty("only-in-derived", "d");
        check("Properties own value", "from-derived", derived.getProperty("shared"));
        check("Properties inherited value", "b", derived.getProperty("only-in-base"));
        check("Properties missing", "null", String.valueOf(derived.getProperty("absent")));
        check("Properties default arg", "fallback",
                derived.getProperty("absent", "fallback"));
        check("Properties size excludes defaults", "2", String.valueOf(derived.size()));
        check("Properties containsKey", "true",
                String.valueOf(derived.containsKey("only-in-derived")));
        check("Properties keySet size", "2", String.valueOf(derived.keySet().size()));
        derived.remove("only-in-derived");
        check("Properties after remove", "1", String.valueOf(derived.size()));
        Properties standalone = new Properties();
        check("empty Properties size", "0", String.valueOf(standalone.size()));
        check("empty Properties get", "null", String.valueOf(standalone.getProperty("k")));
        standalone.setProperty("k", "v");
        check("empty Properties then set", "v", standalone.getProperty("k"));
    }

    static void emptyConcurrentReads() {
        java.util.concurrent.ConcurrentHashMap<String, String> m =
                new java.util.concurrent.ConcurrentHashMap<>();
        check("empty CHM size", "0", String.valueOf(m.size()));
        check("empty CHM isEmpty", "true", String.valueOf(m.isEmpty()));
        check("empty CHM get", "null", String.valueOf(m.get("k")));
        check("empty CHM containsKey", "false", String.valueOf(m.containsKey("k")));
        check("empty CHM containsValue", "false", String.valueOf(m.containsValue("v")));
        check("empty CHM remove", "null", String.valueOf(m.remove("k")));
        check("empty CHM keySet", "0", String.valueOf(m.keySet().size()));
        check("empty CHM values", "0", String.valueOf(m.values().size()));
        check("empty CHM entrySet", "0", String.valueOf(m.entrySet().size()));
        check("empty CHM iterator", "false",
                String.valueOf(m.entrySet().iterator().hasNext()));
        check("empty CHM toString", "{}", m.toString());
        check("empty CHM getOrDefault", "d", m.getOrDefault("k", "d"));
        check("empty CHM replace is null", "null", String.valueOf(m.replace("k", "v")));

        // Each inserting door, on its OWN never-written map.
        java.util.concurrent.ConcurrentHashMap<String, String> a =
                new java.util.concurrent.ConcurrentHashMap<>();
        a.put("k", "v");
        check("empty CHM put then get", "v", a.get("k"));
        check("empty CHM put then size", "1", String.valueOf(a.size()));

        java.util.concurrent.ConcurrentHashMap<String, String> b =
                new java.util.concurrent.ConcurrentHashMap<>();
        check("empty CHM putIfAbsent returns null", "null",
                String.valueOf(b.putIfAbsent("k", "v")));
        check("empty CHM putIfAbsent stored", "v", b.get("k"));

        java.util.concurrent.ConcurrentHashMap<String, String> c =
                new java.util.concurrent.ConcurrentHashMap<>();
        check("empty CHM computeIfAbsent", "v", c.computeIfAbsent("k", x -> "v"));
        check("empty CHM computeIfAbsent stored", "v", c.get("k"));
        check("empty CHM computeIfAbsent size", "1", String.valueOf(c.size()));

        java.util.concurrent.ConcurrentHashMap<String, String> d =
                new java.util.concurrent.ConcurrentHashMap<>();
        check("empty CHM merge", "v", d.merge("k", "v", (x, y) -> x + y));
        check("empty CHM merge stored", "v", d.get("k"));

        java.util.concurrent.ConcurrentHashMap<String, String> e =
                new java.util.concurrent.ConcurrentHashMap<>();
        check("empty CHM compute", "v", e.compute("k", (x, y) -> "v"));
        check("empty CHM compute stored", "v", e.get("k"));

        java.util.concurrent.ConcurrentHashMap<String, String> f =
                new java.util.concurrent.ConcurrentHashMap<>();
        f.putAll(java.util.Map.of("k", "v"));
        check("empty CHM putAll stored", "v", f.get("k"));

        // Growth past the first table, on a map that started with none.
        java.util.concurrent.ConcurrentHashMap<String, String> g =
                new java.util.concurrent.ConcurrentHashMap<>();
        for (int i = 0; i < 64; i++) {
            g.put("g" + i, "v" + i);
        }
        check("empty CHM grown size", "64", String.valueOf(g.size()));
        check("empty CHM grown get first", "v0", g.get("g0"));
        check("empty CHM grown get last", "v63", g.get("g63"));
        check("empty CHM grown keySet", "64", String.valueOf(g.keySet().size()));
    }

    static void check(String what, String expected, String actual) {
        if (!expected.equals(actual)) {
            System.out.println("  MISMATCH " + what + ": expected <" + expected
                    + "> got <" + actual + ">");
            failures++;
        }
    }
}
