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
 * See docs/known-issues/perf/jdk-collection-classes-are-padded-to-a-synthetic-stub-floor-20260911.md
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

    static void check(String what, String expected, String actual) {
        if (!expected.equals(actual)) {
            System.out.println("  MISMATCH " + what + ": expected <" + expected
                    + "> got <" + actual + ">");
            failures++;
        }
    }
}
