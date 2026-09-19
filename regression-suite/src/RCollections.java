import java.util.*;

/**
 * Regression: java.util collections — the area with the most native-intrinsic
 * surface in CratonVM. Exercises ArrayList (incl. subList view + toArray(T[])),
 * type-strict wrapper keys, ordered/sorted maps, and iterator semantics.
 */
public class RCollections {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    public static void main(String[] a) {
        // ---- ArrayList + subList view (regressed once: subList.toArray(T[])) ----
        List<Integer> l = new ArrayList<>();
        for (int i = 0; i < 10; i++) l.add(i);
        List<Integer> sub = l.subList(2, 7);            // [2,3,4,5,6]
        check(sub.size() == 5, "subList size");
        check(sub.get(0) == 2 && sub.get(4) == 6, "subList get");
        check(sub.contains(4) && !sub.contains(9), "subList contains");
        check(sub.indexOf(5) == 3, "subList indexOf");
        Integer[] arr = sub.toArray(new Integer[0]);    // toArray(T[]) overload
        check(arr.length == 5 && arr[0] == 2 && arr[4] == 6, "subList toArray(T[])");
        Object[] oarr = sub.toArray();                   // no-arg toArray
        check(oarr.length == 5 && oarr[2].equals(4), "subList toArray()");
        int s = 0; for (int x : sub) s += x; check(s == 20, "subList iterator");
        check(new ArrayList<>(sub).equals(Arrays.asList(2,3,4,5,6)), "subList snapshot");

        // ---- ArrayList core ----
        l.add(2, 99); check(l.get(2) == 99 && l.size() == 11, "add(idx)");
        l.remove(Integer.valueOf(99)); check(l.size() == 10, "remove(obj)");
        Collections.sort(l, Collections.reverseOrder());
        check(l.get(0) == 9 && l.get(9) == 0, "sort reverse");

        // ---- HashMap with boxed + String keys ----
        // (NOTE: strict cross-type wrapper-key identity — Integer(1) != Long(1) —
        //  is a known CratonVM divergence; see README "Known gaps". Not asserted.)
        Map<Object, String> m = new HashMap<>();
        m.put(Integer.valueOf(1), "one");
        m.put("k", "v");
        m.put(Integer.valueOf(257), "big"); // outside the Integer cache
        check(m.size() == 3, "HashMap size");
        check("one".equals(m.get(1)) && "big".equals(m.get(257)), "Integer keys");
        check("v".equals(m.get("k")) && m.get(2) == null, "String key + absent");
        check(m.computeIfAbsent("z", k -> "zz").equals("zz") && m.get("z").equals("zz"), "computeIfAbsent");
        check(m.getOrDefault("nope", "d").equals("d"), "getOrDefault");

        // ---- TreeMap custom comparator + navigation ----
        TreeMap<String, Integer> t = new TreeMap<>(Comparator.reverseOrder());
        t.put("a", 1); t.put("c", 3); t.put("b", 2);
        check(t.firstKey().equals("c") && t.lastKey().equals("a"), "TreeMap reverse order");
        check(t.headMap("b").size() == 1, "TreeMap headMap");
        TreeMap<Integer, Integer> t2 = new TreeMap<>();
        for (int i = 10; i >= 1; i--) t2.put(i, i * i);
        check(t2.firstKey() == 1 && t2.get(7) == 49, "TreeMap natural order");

        // ---- LinkedHashMap insertion order ----
        LinkedHashMap<String, Integer> lh = new LinkedHashMap<>();
        lh.put("x", 1); lh.put("y", 2); lh.put("z", 3); lh.put("x", 11);
        check(new ArrayList<>(lh.keySet()).equals(Arrays.asList("x", "y", "z")), "LHM order");
        check(lh.get("x") == 11, "LHM update");

        // ---- Sets + Deque ----
        Set<Integer> set = new LinkedHashSet<>(Arrays.asList(3, 1, 2, 1, 3));
        check(set.size() == 3 && new ArrayList<>(set).equals(Arrays.asList(3,1,2)), "LinkedHashSet");
        Deque<Integer> dq = new ArrayDeque<>();
        dq.addFirst(1); dq.addLast(2); dq.addFirst(0);
        check(dq.peekFirst() == 0 && dq.peekLast() == 2 && dq.size() == 3, "ArrayDeque");

        // ---- Iterator.remove (structural mutation through the iterator) ----
        // (NOTE: fail-fast ConcurrentModificationException detection is a known
        //  CratonVM gap; see README. We test the supported Iterator.remove path.)
        List<Integer> rm = new ArrayList<>(Arrays.asList(1, 2, 3, 4, 5, 6));
        Iterator<Integer> it = rm.iterator();
        while (it.hasNext()) { if (it.next() % 2 == 0) it.remove(); }
        check(rm.equals(Arrays.asList(1, 3, 5)), "Iterator.remove");

        // ---- List.of / Map.of (immutable) ----
        List<Integer> imm = List.of(1, 2, 3);
        check(imm.size() == 3, "List.of");
        boolean uoe = false;
        try { imm.add(4); } catch (UnsupportedOperationException e) { uoe = true; }
        check(uoe, "immutable List.of");

        // ---- HashSet bulk ops with a FOREIGN collection argument (HIB-CV-25) ----
        // A custom Set whose elements live in named fields (element1/element2),
        // not an ArrayList-shaped backing array — mirrors Weld's
        // ImmutableTinySet$Doubleton. CratonVM's HashSet.containsAll/removeAll/
        // retainAll must extract such an argument's elements via its real
        // iterator()/toArray(), NOT silently see it as empty. An empty-seen arg
        // made containsAll() vacuously true, which (via AbstractSet.equals ->
        // other.containsAll(this)) corrupted Set-keyed map lookups and broke
        // Weld's CDI qualifier discovery.
        Set<String> hs = new HashSet<>(Arrays.asList("a", "b"));
        Set<String> foreignDisjoint = new TinySet("c", "d");
        Set<String> foreignPartial  = new TinySet("a", "c");   // shares "a" only
        Set<String> foreignSubset   = new TinySet("a", "b");
        check(!hs.containsAll(foreignDisjoint), "containsAll(foreign disjoint)");
        check(!hs.containsAll(foreignPartial),  "containsAll(foreign partial)");
        check(hs.containsAll(foreignSubset),    "containsAll(foreign subset)");
        // AbstractSet.equals across the type boundary must be symmetric+correct.
        check(!hs.equals(foreignPartial) && !foreignPartial.equals(hs), "equals foreign partial");
        check(hs.equals(foreignSubset) && foreignSubset.equals(hs), "equals foreign subset");
        // Set.hashCode contract across the type boundary: a foreign Set's
        // inherited AbstractSet.hashCode() is the SUM of its element
        // hashCodes, so it must equal an equal HashSet's. CratonVM routes
        // AbstractSet.hashCode through its HashSet native, which saw a
        // foreign (non-backing-map) layout as empty and returned 0 — equal
        // sets that hash differently are unfindable in any HashMap, which is
        // what the "set-key true hit" check below then hit as a null return.
        check(new TinySet("a", "x").hashCode() == "a".hashCode() + "x".hashCode(),
                "foreign Set hashCode is element-hash sum");
        check(foreignSubset.hashCode() == hs.hashCode(), "foreign/HashSet hashCode agree");
        check(new TinySet("a", "b").hashCode() == new HashSet<>(Arrays.asList("a", "b")).hashCode(),
                "foreign/HashSet hashCode agree (fresh)");
        // Set-of-set keyed map: a HashSet lookup must not collide with a foreign
        // set key that merely shares one element (the getSharedSet failure) —
        // and must HIT when the sets are equal, in both key directions.
        Map<Set<String>, String> byKey = new HashMap<>();
        byKey.put(new TinySet("a", "x"), "AX");
        check(byKey.get(new HashSet<>(Arrays.asList("a", "y"))) == null, "set-key no false hit");
        check("AX".equals(byKey.get(new HashSet<>(Arrays.asList("a", "x")))), "set-key true hit");
        // Reverse: HashSet key, foreign-Set probe.
        Map<Set<String>, String> byKeyRev = new HashMap<>();
        byKeyRev.put(new HashSet<>(Arrays.asList("a", "x")), "AX");
        check("AX".equals(byKeyRev.get(new TinySet("a", "x"))), "set-key true hit (reverse)");
        check(byKeyRev.get(new TinySet("a", "y")) == null, "set-key no false hit (reverse)");
        // retainAll against a foreign arg must keep the shared element, not empty.
        Set<String> retain = new HashSet<>(Arrays.asList("a", "b"));
        retain.retainAll(new TinySet("a", "z"));
        check(retain.equals(new HashSet<>(Arrays.asList("a"))), "retainAll(foreign)");
        // removeAll against a foreign arg must remove the shared element.
        Set<String> remove = new HashSet<>(Arrays.asList("a", "b"));
        remove.removeAll(new TinySet("a", "z"));
        check(remove.equals(new HashSet<>(Arrays.asList("b"))), "removeAll(foreign)");

        // ---- Foreign OPEN-ADDRESSED Set argument: null holes in the probed
        // slots (WildFly/MSC IdentityHashSet). HoleySet's field layout is
        // exactly the (Object[] table, int size) shape CratonVM's
        // collection-layout heuristic reads as "dense arr[0..size) prefix", but
        // its live elements sit at hash positions with nulls in between. The
        // heuristic therefore returns the right element COUNT made of mostly
        // nulls, which looks plausible and only fails when a caller
        // dereferences one: HashSet.addAll(mscIdentityHashSet) produced
        // {null} from a 3-element set, so WildFly's ContainerStateMonitor
        // iterated a null ServiceController and every boot with a failed
        // service died on a NullPointerException instead of logging a report.
        Set<String> holey = new HoleySet("a", "b", "c");
        check(holey.size() == 3, "HoleySet size");
        Set<String> copied = new HashSet<>();
        copied.addAll(holey);
        check(!copied.contains(null), "addAll(open-addressed foreign) has no null holes");
        check(copied.equals(new HashSet<>(Arrays.asList("a", "b", "c"))), "addAll(open-addressed foreign)");
        check(new HashSet<>(holey).equals(copied), "HashSet(open-addressed foreign) ctor");
        Object[] holeyArr = holey.toArray();
        check(holeyArr.length == 3, "open-addressed foreign toArray length");
        for (Object o : holeyArr) { check(o != null, "open-addressed foreign toArray non-null"); }
        check(copied.containsAll(holey), "containsAll(open-addressed foreign)");
        Set<String> retainHoley = new HashSet<>(Arrays.asList("a", "z"));
        retainHoley.retainAll(holey);
        check(retainHoley.equals(new HashSet<>(Arrays.asList("a"))), "retainAll(open-addressed foreign)");
        Set<String> removeHoley = new HashSet<>(Arrays.asList("a", "z"));
        removeHoley.removeAll(holey);
        check(removeHoley.equals(new HashSet<>(Arrays.asList("z"))), "removeAll(open-addressed foreign)");
        // A List may legitimately hold nulls at any index, so the guard above
        // must not "repair" one.
        List<String> withNulls = new ArrayList<>(Arrays.asList("a", null, "b"));
        check(new ArrayList<>(withNulls).equals(withNulls), "List with null elements preserved");
        check(withNulls.toArray().length == 3 && withNulls.toArray()[1] == null,
                "List toArray keeps null element");

        System.out.println("PASS RCollections (" + checks + " checks)");
    }

    /**
     * A minimal immutable two-element Set whose members are stored in named
     * fields (not an array) — deliberately shaped like Weld's
     * {@code ImmutableTinySet$Doubleton} so CratonVM's collection-layout
     * heuristics cannot model it and must fall back to the real iterator.
     */
    static final class TinySet extends AbstractSet<String> {
        private final String element1, element2;
        TinySet(String e1, String e2) { this.element1 = e1; this.element2 = e2; }
        public int size() { return 2; }
        public boolean contains(Object o) { return element1.equals(o) || element2.equals(o); }
        public Iterator<String> iterator() {
            return Arrays.asList(element1, element2).iterator();
        }
    }

    /**
     * A minimal open-addressed Set: {@code (Object[] table, int size)} — the
     * exact field shape CratonVM's collection-layout heuristic treats as a
     * dense {@code table[0..size)} prefix — but with the live elements scattered
     * across hash positions and NULL holes in between. Deliberately shaped like
     * {@code org.jboss.msc.service.IdentityHashSet} (and Kafka's
     * {@code ImplicitLinkedHashCollection}) so a heuristic snapshot of it is
     * wrong in the silent way: right count, mostly nulls.
     */
    static final class HoleySet extends AbstractSet<String> {
        private final Object[] table;
        private final int size;
        HoleySet(String... elements) {
            this.table = new Object[16];
            for (String e : elements) {
                int i = (e.hashCode() & 0x7fffffff) % table.length;
                while (table[i] != null) { i = (i + 1) % table.length; }
                table[i] = e;
            }
            this.size = elements.length;
        }
        public int size() { return size; }
        public Iterator<String> iterator() {
            List<String> live = new ArrayList<>(size);
            for (Object o : table) { if (o != null) { live.add((String) o); } }
            return live.iterator();
        }
    }
}
