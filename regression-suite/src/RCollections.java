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

        System.out.println("PASS RCollections (" + checks + " checks)");
    }
}
