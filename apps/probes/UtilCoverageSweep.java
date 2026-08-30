import java.util.*;
import java.util.function.*;

/** L3 coverage sweep — the `java.util` registrations the twelve probes never reached.
 *
 *  Built from a registry dump taken DURING all sixteen existing probes and
 *  merged: of 602 owning registrations over a real JDK body, 445 were exercised
 *  and 157 were not. Most of that 157 turned out to be coverage gaps rather
 *  than dead code — the NavigableSet surface of `TreeMap.keySet()`, HashMap's
 *  conditional mutators, the sublist ListIterator — which is what this probe
 *  closes.
 *
 *  Every call is written as a lambda, never `x::m`: a bound method reference is
 *  a different dispatch door on this VM (see the companion record), so `t(tag,
 *  x::m)` would measure the door rather than the family.
 */
public class UtilCoverageSweep {
    static int rows = 0;

    interface ThrowingRun { void run() throws Throwable; }

    static String esc(String s) {
        if (s == null) return "null";
        return s.replace("\n", "\\n").replace("\r", "\\r");
    }

    static void p(String tag, Object v) {
        rows++;
        System.out.println(rows + " " + esc(tag) + " |" + esc(String.valueOf(v)) + "|");
    }

    static void t(String tag, ThrowingRun r) {
        try {
            r.run();
            p(tag, "no-throw");
        } catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName());
        }
    }

    /** Value-or-throw: prints the value when it comes back, the type when it does not. */
    static void tv(String tag, Callable c) {
        try {
            p(tag, c.call());
        } catch (Throwable e) {
            p(tag, "THREW " + e.getClass().getName());
        }
    }

    interface Callable { Object call() throws Throwable; }

    static TreeMap<String, Integer> tm() {
        TreeMap<String, Integer> m = new TreeMap<>();
        m.put("b", 2);
        m.put("d", 4);
        m.put("f", 6);
        m.put("h", 8);
        return m;
    }

    // ---------------------------------------------------------------- keySet
    // The NavigableSet surface of `TreeMap.keySet()`: 18 registrations that a
    // 300-row map-view battery never reached, because it asked Set questions
    // and these are NavigableSet ones.
    static void treeMapKeySet() {
        NavigableSet<String> ks = tm().navigableKeySet();
        p("ks class", ks.getClass().getName());
        p("ks first", ks.first());
        p("ks last", ks.last());
        p("ks ceiling c", ks.ceiling("c"));
        p("ks ceiling d", ks.ceiling("d"));
        p("ks ceiling z", ks.ceiling("z"));
        p("ks floor c", ks.floor("c"));
        p("ks floor b", ks.floor("b"));
        p("ks floor a", ks.floor("a"));
        p("ks higher d", ks.higher("d"));
        p("ks higher h", ks.higher("h"));
        p("ks lower d", ks.lower("d"));
        p("ks lower b", ks.lower("b"));
        p("ks comparator", ks.comparator());
        p("ks headSet d", ks.headSet("d").toString());
        p("ks headSet d inc", ks.headSet("d", true).toString());
        p("ks tailSet d", ks.tailSet("d").toString());
        p("ks tailSet d exc", ks.tailSet("d", false).toString());
        p("ks subSet b-f", ks.subSet("b", "f").toString());
        p("ks subSet b-f both inc", ks.subSet("b", true, "f", true).toString());
        p("ks descendingSet", ks.descendingSet().toString());

        StringBuilder di = new StringBuilder();
        Iterator<String> it = ks.descendingIterator();
        while (it.hasNext()) di.append(it.next()).append(',');
        p("ks descendingIterator", di.toString());

        p("ks spliterator chars", ks.spliterator().characteristics());
        p("ks spliterator size", ks.spliterator().estimateSize());

        NavigableSet<String> pk = tm().navigableKeySet();
        p("ks pollFirst", pk.pollFirst());
        p("ks pollLast", pk.pollLast());
        p("ks after polls", pk.toString());

        NavigableSet<String> empty = new TreeMap<String, Integer>().navigableKeySet();
        p("ks empty pollFirst", empty.pollFirst());
        tv("ks empty first", () -> empty.first());
    }

    // ------------------------------------------------------------- HashMap
    // The conditional mutators and the functional surface, on a PLAIN HashMap:
    // the corpus exercised these on Properties, LinkedHashMap and TreeMap and
    // never on the family they are registered for.
    static void hashMap() {
        Map<String, Integer> m = new HashMap<>();
        m.put("a", 1);
        m.put("b", 2);

        p("hm putIfAbsent present", m.putIfAbsent("a", 9));
        p("hm putIfAbsent absent", m.putIfAbsent("c", 3));
        p("hm after putIfAbsent", new TreeMap<>(m).toString());

        p("hm replace present", m.replace("a", 11));
        p("hm replace absent", m.replace("zz", 1));
        p("hm replace kvv match", m.replace("b", 2, 22));
        p("hm replace kvv mismatch", m.replace("b", 999, 33));
        p("hm after replace", new TreeMap<>(m).toString());

        p("hm remove kv match", m.remove("c", 3));
        p("hm remove kv mismatch", m.remove("a", 999));
        p("hm after remove kv", new TreeMap<>(m).toString());

        p("hm computeIfAbsent new", m.computeIfAbsent("d", k -> 4));
        p("hm computeIfAbsent existing", m.computeIfAbsent("a", k -> 99));
        p("hm computeIfAbsent null fn result", m.computeIfAbsent("e", k -> null));
        p("hm computeIfPresent present", m.computeIfPresent("d", (k, v) -> v + 10));
        p("hm computeIfPresent absent", m.computeIfPresent("zz", (k, v) -> 1));
        p("hm computeIfPresent to null", m.computeIfPresent("d", (k, v) -> null));
        p("hm after computes", new TreeMap<>(m).toString());

        StringBuilder fe = new StringBuilder();
        new TreeMap<>(m).forEach((k, v) -> fe.append(k).append('=').append(v).append(','));
        p("hm forEach order-independent", fe.toString());

        Map<String, Integer> ra = new HashMap<>(m);
        ra.replaceAll((k, v) -> v * 2);
        p("hm replaceAll", new TreeMap<>(ra).toString());

        Map<String, Integer> cap = new HashMap<>(8, 0.5f);
        cap.put("x", 1);
        p("hm ctor cap+load", cap.toString());
        t("hm ctor negative cap", () -> new HashMap<String, Integer>(-1, 0.5f));
        t("hm ctor zero load", () -> new HashMap<String, Integer>(8, 0f));
        t("hm ctor NaN load", () -> new HashMap<String, Integer>(8, Float.NaN));
    }

    // ------------------------------------------------------- sublist + itr
    static void subList() {
        List<String> base = new ArrayList<>(Arrays.asList("a", "b", "c", "d", "e", "f"));
        List<String> sub = base.subList(1, 5);
        p("sub", sub.toString());
        p("sub lastIndexOf", sub.lastIndexOf("d"));
        p("sub lastIndexOf absent", sub.lastIndexOf("zz"));
        p("sub spliterator size", sub.spliterator().estimateSize());
        p("sub spliterator chars", sub.spliterator().characteristics());

        List<String> b2 = new ArrayList<>(Arrays.asList("a", "b", "c", "d", "e", "f"));
        List<String> s2 = b2.subList(1, 5);
        p("sub removeAll", s2.removeAll(Arrays.asList("c", "e")));
        p("sub after removeAll", s2.toString());
        p("base after removeAll", b2.toString());

        List<String> b3 = new ArrayList<>(Arrays.asList("a", "b", "c", "d", "e", "f"));
        List<String> s3 = b3.subList(1, 5);
        p("sub retainAll", s3.retainAll(Arrays.asList("b", "d")));
        p("sub after retainAll", s3.toString());
        p("base after retainAll", b3.toString());

        List<String> b4 = new ArrayList<>(Arrays.asList("a", "b", "c", "d", "e", "f"));
        List<String> s4 = b4.subList(1, 5);
        s4.replaceAll(x -> x.toUpperCase());
        p("sub after replaceAll", s4.toString());
        p("base after replaceAll", b4.toString());

        // The sublist's own ListIterator — seven registrations, none reached.
        List<String> b5 = new ArrayList<>(Arrays.asList("a", "b", "c", "d", "e", "f"));
        ListIterator<String> li = b5.subList(1, 5).listIterator();
        p("li hasPrevious at start", li.hasPrevious());
        p("li nextIndex at start", li.nextIndex());
        p("li previousIndex at start", li.previousIndex());
        p("li next", li.next());
        p("li next 2", li.next());
        p("li hasPrevious", li.hasPrevious());
        p("li previousIndex", li.previousIndex());
        p("li nextIndex", li.nextIndex());
        p("li previous", li.previous());
        li.set("Z");
        p("li after set", b5.toString());
        li.add("Q");
        p("li after add", b5.toString());

        StringBuilder fr = new StringBuilder();
        b5.subList(1, 4).listIterator().forEachRemaining(x -> fr.append(x).append(','));
        p("li forEachRemaining", fr.toString());
    }

    // ------------------------------------------------------------- the rest
    static void tail() {
        Set<String> hs = new HashSet<>(8, 0.5f);
        hs.add("a");
        hs.add("b");
        p("hs ctor cap+load size", hs.size());
        p("hs toArray typed", Arrays.toString(new TreeSet<>(hs).toArray(new String[0])));
        p("hs toArray typed bigger", new TreeSet<>(hs).toArray(new String[5]).length);
        hs.clear();
        p("hs after clear", hs.size());

        LinkedHashSet<String> lhs = new LinkedHashSet<>(8, 0.5f);
        lhs.add("a");
        lhs.add("b");
        p("lhs ctor cap+load", lhs.toString());
        p("lhs spliterator chars", lhs.spliterator().characteristics());
        p("lhs spliterator size", lhs.spliterator().estimateSize());

        LinkedList<String> ll = new LinkedList<>(Arrays.asList("a", "b", "c"));
        p("ll pollFirst", ll.pollFirst());
        p("ll after pollFirst", ll.toString());
        p("ll spliterator size", ll.spliterator().estimateSize());
        p("ll spliterator chars", ll.spliterator().characteristics());
        ll.clear();
        p("ll after clear", ll.toString());
        p("ll pollFirst empty", ll.pollFirst());

        TreeSet<String> ts = new TreeSet<>(Arrays.asList("b", "d", "f", "h"));
        p("ts subSet inc", ts.subSet("b", true, "f", true).toString());
        p("ts subSet exc", ts.subSet("b", false, "f", false).toString());
        p("ts tailSet inc", ts.tailSet("d", true).toString());
        p("ts tailSet exc", ts.tailSet("d", false).toString());
        ts.clear();
        p("ts after clear", ts.toString());

        TreeMap<String, Integer> t2 = tm();
        p("tm compute new", t2.compute("z", (k, v) -> 26));
        p("tm compute existing", t2.compute("b", (k, v) -> v + 100));
        p("tm compute to null", t2.compute("d", (k, v) -> null));
        p("tm computeIfPresent", t2.computeIfPresent("f", (k, v) -> v + 1));
        p("tm computeIfPresent absent", t2.computeIfPresent("zz", (k, v) -> 1));
        p("tm replace kvv match", t2.replace("h", 8, 88));
        p("tm replace kvv mismatch", t2.replace("h", 999, 77));
        p("tm after", t2.toString());

        LinkedHashMap<String, Integer> lhm = new LinkedHashMap<>();
        lhm.put("a", 1);
        lhm.put("b", 2);
        lhm.clear();
        p("lhm after clear", lhm.toString());
        p("lhm size after clear", lhm.size());

        Hashtable<String, Integer> ht = new Hashtable<>();
        ht.put("a", 1);
        p("ht remove kv mismatch", ht.remove("a", 999));
        p("ht remove kv match", ht.remove("a", 1));
        p("ht after", ht.toString());

        ArrayDeque<String> ad = new ArrayDeque<>(Arrays.asList("a", "b", "c"));
        StringBuilder adf = new StringBuilder();
        ad.forEach(x -> adf.append(x).append(','));
        p("ad forEach", adf.toString());
    }

    /** The spliterator characteristics matrix.
     *
     *  `SPL_LINKED_SET` is one constant shared by two producers -- a
     *  `LinkedHashSet` and a `LinkedHashMap` view -- and the JDK builds those
     *  two different ways: `LinkedHashSet.spliterator()` goes through
     *  `Spliterators.spliterator(Collection, ..)`, which ADDS `SIZED|SUBSIZED`,
     *  while the map's views have their own spliterator classes. Two spellings
     *  are not one contract, so this asks every cell rather than assuming the
     *  one that failed generalises.
     */
    static void spliteratorMatrix() {
        Map<String, Integer> hm = new HashMap<>();
        hm.put("a", 1); hm.put("b", 2);
        LinkedHashMap<String, Integer> lhm = new LinkedHashMap<>();
        lhm.put("a", 1); lhm.put("b", 2);
        TreeMap<String, Integer> tmm = new TreeMap<>();
        tmm.put("a", 1); tmm.put("b", 2);

        p("spl HashSet", new HashSet<>(Arrays.asList("a", "b")).spliterator().characteristics());
        p("spl LinkedHashSet", new LinkedHashSet<>(Arrays.asList("a", "b")).spliterator().characteristics());
        p("spl TreeSet", new TreeSet<>(Arrays.asList("a", "b")).spliterator().characteristics());

        p("spl HashMap keySet", hm.keySet().spliterator().characteristics());
        p("spl HashMap values", hm.values().spliterator().characteristics());
        p("spl HashMap entrySet", hm.entrySet().spliterator().characteristics());

        p("spl LHM keySet", lhm.keySet().spliterator().characteristics());
        p("spl LHM values", lhm.values().spliterator().characteristics());
        p("spl LHM entrySet", lhm.entrySet().spliterator().characteristics());

        p("spl TreeMap keySet", tmm.keySet().spliterator().characteristics());
        p("spl TreeMap values", tmm.values().spliterator().characteristics());
        p("spl TreeMap entrySet", tmm.entrySet().spliterator().characteristics());

        Hashtable<String, Integer> htt = new Hashtable<>();
        htt.put("a", 1); htt.put("b", 2);
        p("spl Hashtable keySet", htt.keySet().spliterator().characteristics());
        p("spl Hashtable values", htt.values().spliterator().characteristics());
        p("spl Hashtable entrySet", htt.entrySet().spliterator().characteristics());

        Properties prr = new Properties();
        prr.setProperty("a", "1"); prr.setProperty("b", "2");
        p("spl Properties keySet", prr.keySet().spliterator().characteristics());
        p("spl Properties values", prr.values().spliterator().characteristics());
        p("spl Properties entrySet", prr.entrySet().spliterator().characteristics());

        p("spl Vector", new Vector<>(Arrays.asList("a", "b")).spliterator().characteristics());
        p("spl Stack", new Stack<String>().spliterator().characteristics());
        p("spl EnumSet-like Arrays.asList", Arrays.asList("a", "b").spliterator().characteristics());
        p("spl List.of", List.of("a", "b").spliterator().characteristics());
        p("spl Set.of", Set.of("a").spliterator().characteristics());
        // Every immutable shape, so the fix is one measured rule and not one
        // measured cell: Set12 vs SetN, List12 vs ListN, the map views, and the
        // empty singletons.
        p("spl Set.of()", Set.of().spliterator().characteristics());
        p("spl Set.of(1,2)", Set.of("a", "b").spliterator().characteristics());
        p("spl Set.of x3 (SetN)", Set.of("a", "b", "c").spliterator().characteristics());
        p("spl List.of()", List.of().spliterator().characteristics());
        p("spl List.of(1)", List.of("a").spliterator().characteristics());
        p("spl List.of x3", List.of("a", "b", "c").spliterator().characteristics());
        p("spl Map.of keySet", Map.of("a", 1).keySet().spliterator().characteristics());
        p("spl Map.of values", Map.of("a", 1).values().spliterator().characteristics());
        p("spl Map.of entrySet", Map.of("a", 1).entrySet().spliterator().characteristics());
        p("spl Collections.singleton", Collections.singleton("a").spliterator().characteristics());
        p("spl Collections.emptySet", Collections.emptySet().spliterator().characteristics());
        p("spl unmodifiableSet", Collections.unmodifiableSet(
                new HashSet<>(Arrays.asList("a"))).spliterator().characteristics());
        p("spl unmodifiableList", Collections.unmodifiableList(
                new ArrayList<>(Arrays.asList("a"))).spliterator().characteristics());
        p("Set.of class", Set.of("a").getClass().getName());
        p("List.of class", List.of("a").getClass().getName());
        p("spl Collections.emptyList", Collections.emptyList().spliterator().characteristics());

        p("spl ArrayList", new ArrayList<>(Arrays.asList("a", "b")).spliterator().characteristics());
        p("spl LinkedList", new LinkedList<>(Arrays.asList("a", "b")).spliterator().characteristics());
        p("spl ArrayDeque", new ArrayDeque<>(Arrays.asList("a", "b")).spliterator().characteristics());
        p("spl PriorityQueue", new PriorityQueue<>(Arrays.asList("a", "b")).spliterator().characteristics());
    }

    public static void main(String[] a) {
        spliteratorMatrix();
        treeMapKeySet();
        hashMap();
        subList();
        tail();
        System.out.println("DONE UtilCoverageSweep");
    }
}
