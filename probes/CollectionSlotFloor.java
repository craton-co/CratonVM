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
 * KNOWN BASELINE, synthetic-JDK mode, 2026-09-11: eight checks mismatch there —
 * `TreeMap.keySet()/entrySet()` report 0, `IdentityHashMap` reports 0 for
 * everything, and `LinkedHashSet` reports 0 for size. Those are synthetic-JDK
 * gaps in three collection families that have nothing to do with the slot
 * floors: an unchanged build produces the IDENTICAL eight. Real-JDK mode is
 * clean. Compare the SET of mismatches against that baseline rather than
 * expecting a bare pass in synthetic mode.
 *
 * See docs/internal/fixed-bugs/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-FIXED-20260911.md
 */
public class CollectionSlotFloor {
    static int failures = 0;

    public static void main(String[] args) {
        // Maps: buckets / size / capacity, plus growth past the initial table.
        mapFamily("HashMap", new HashMap<String, String>());
        mapFamily("Hashtable", new Hashtable<String, String>());
        mapFamily("LinkedHashMap", new LinkedHashMap<String, String>());
        mapFamily("ConcurrentHashMap", new java.util.concurrent.ConcurrentHashMap<String, String>());
        mapFamily("TreeMap", new TreeMap<String, String>());
        // NOT through `mapFamily`: IdentityHashMap keys on reference identity, so
        // a lookup with an equal-but-distinct String is a legitimate miss there.
        // Its slots get their own check below.
        identityMap();

        setFamily("HashSet", new HashSet<String>());
        setFamily("LinkedHashSet", new LinkedHashSet<String>());
        setFamily("TreeSet", new TreeSet<String>());

        listFamily("ArrayList", new ArrayList<String>());
        listFamily("Vector", new Vector<String>());
        listFamily("LinkedList", new LinkedList<String>());
        listFamily("ArrayDeque", new ArrayDeque<String>());
        listFamily("CopyOnWriteArrayList", new java.util.concurrent.CopyOnWriteArrayList<String>());

        // LinkedHashMap's head/tail slots (3 and 4) are what make it ORDERED.
        // A lost write there reads as a HashMap with the right contents and the
        // wrong iteration order -- the quietest failure in this whole file.
        LinkedHashMap<String, Integer> ordered = new LinkedHashMap<>();
        for (int i = 0; i < 12; i++) ordered.put("k" + i, i);
        StringBuilder sb = new StringBuilder();
        for (Map.Entry<String, Integer> e : ordered.entrySet()) sb.append(e.getValue()).append(',');
        check("LinkedHashMap insertion order", "0,1,2,3,4,5,6,7,8,9,10,11,", sb.toString());

        // The EMPTY read path, for the sorted families.
        //
        // Every section above fills its collection first, so none of them ever
        // reads one that has no backing array. `TreeMap`/`TreeSet` install
        // theirs on the first insert (`native_tm_put` / `native_ts_add`), which
        // makes "never written" a state the readers have to handle rather than
        // a state they happen never to see -- and a reader that assumes an
        // array reads a fresh map as a crash or as garbage, not as empty.
        emptySortedReads();
        emptyConcurrentReads();

        // EnumMap / EnumSet have their own floors.
        EnumMap<Day, String> em = new EnumMap<>(Day.class);
        em.put(Day.WED, "w");
        em.put(Day.MON, "m");
        check("EnumMap size", "2", String.valueOf(em.size()));
        check("EnumMap get", "w", String.valueOf(em.get(Day.WED)));
        check("EnumMap key order", "[MON, WED]", em.keySet().toString());
        EnumSet<Day> es = EnumSet.of(Day.FRI, Day.MON);
        check("EnumSet size", "2", String.valueOf(es.size()));
        check("EnumSet contains", "true", String.valueOf(es.contains(Day.FRI)));

        PriorityQueue<Integer> pq = new PriorityQueue<>(List.of(5, 1, 4, 2, 3));
        check("PriorityQueue head", "1", String.valueOf(pq.peek()));
        check("PriorityQueue size", "5", String.valueOf(pq.size()));

        StringJoiner sj = new StringJoiner(",", "[", "]");
        sj.add("a").add("b");
        check("StringJoiner", "[a,b]", sj.toString());

        System.out.println(failures == 0 ? "PASS CollectionSlotFloor"
                                         : "FAIL CollectionSlotFloor (" + failures + ")");
        System.out.println("SLOTFLOOR_END");
        if (failures != 0) System.exit(1);
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
