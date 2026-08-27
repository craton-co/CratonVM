import java.util.*;

/**
 * Which iterator CLASS does each collection hand out?
 *
 * `ArrayList$Itr.hasNext/next` cannot yield to real JDK bytecode while a view
 * carrier can also be handed one: a values view has no `modCount` slot, so the
 * real `checkForComodification` reads its `this$0` reference as an int and
 * throws a spurious CME. `VALUES_ITR_CARRIERS` already maps four carriers to
 * their own iterator classes; this probe is the census of which receivers are
 * still left on the shared `ArrayList$Itr`, i.e. exactly the work remaining
 * before that retag is safe.
 *
 * Every row that reads `java.util.ArrayList$Itr` on a NON-list receiver is a
 * blocker. Diff against HotSpot to see what each one should be.
 */
public class ItrClassProbe {
    static void p(String k, Object v) { System.out.println("ICP " + k + "=" + v); }
    static void c(String k, Iterable<?> it) { p(k, it.iterator().getClass().getName()); }

    public static void main(String[] args) {
        List<String> al = new ArrayList<>(List.of("a","b"));
        c("arrayList", al);
        c("subList", al.subList(0,1));
        c("arraysAsList", Arrays.asList("a","b"));
        c("unmodList", Collections.unmodifiableList(al));
        c("cow", new java.util.concurrent.CopyOnWriteArrayList<>(al));
        c("linkedList", new LinkedList<>(al));

        Map<String,String> hm = new HashMap<>(); hm.put("k","v");
        c("hm.keySet", hm.keySet()); c("hm.values", hm.values()); c("hm.entrySet", hm.entrySet());
        Map<String,String> lhm = new LinkedHashMap<>(hm);
        c("lhm.keySet", lhm.keySet()); c("lhm.values", lhm.values()); c("lhm.entrySet", lhm.entrySet());
        Map<String,String> tm = new TreeMap<>(hm);
        c("tm.keySet", tm.keySet()); c("tm.values", tm.values()); c("tm.entrySet", tm.entrySet());
        Hashtable<String,String> ht = new Hashtable<>(hm);
        c("ht.keySet", ht.keySet()); c("ht.values", ht.values()); c("ht.entrySet", ht.entrySet());
        Map<String,String> chm = new java.util.concurrent.ConcurrentHashMap<>(hm);
        c("chm.keySet", chm.keySet()); c("chm.values", chm.values()); c("chm.entrySet", chm.entrySet());

        // Families the first cut of this census MISSED, which is how an
        // `IdentityHashMap` values view -- still on the shared `ArrayList$Itr`
        // -- got past it and threw a spurious CME the moment the yield was
        // switched on. A census is only as good as its receiver list.
        Map<String,String> idm = new IdentityHashMap<>(); idm.put("k","v");
        c("idm.keySet", idm.keySet()); c("idm.values", idm.values()); c("idm.entrySet", idm.entrySet());
        Map<String,String> whm = new WeakHashMap<>(); whm.put("k","v");
        c("whm.keySet", whm.keySet()); c("whm.values", whm.values()); c("whm.entrySet", whm.entrySet());
        Map<java.time.DayOfWeek,String> em = new EnumMap<>(java.time.DayOfWeek.class);
        em.put(java.time.DayOfWeek.MONDAY, "v");
        c("em.keySet", em.keySet()); c("em.values", em.values()); c("em.entrySet", em.entrySet());
        Properties pr = new Properties(); pr.setProperty("k","v");
        c("props.keySet", pr.keySet()); c("props.values", pr.values()); c("props.entrySet", pr.entrySet());
        Map<String,String> cslm = new java.util.concurrent.ConcurrentSkipListMap<>(); cslm.put("k","v");
        c("cslm.keySet", cslm.keySet()); c("cslm.values", cslm.values()); c("cslm.entrySet", cslm.entrySet());
        c("cslSet", new java.util.concurrent.ConcurrentSkipListSet<>(al));
        c("copyOnWriteSet", new java.util.concurrent.CopyOnWriteArraySet<>(al));
        c("linkedBlockingQueue", new java.util.concurrent.LinkedBlockingQueue<>(al));
        c("arrayBlockingQueue", new java.util.concurrent.ArrayBlockingQueue<>(4, false, al));
        c("concurrentLinkedQueue", new java.util.concurrent.ConcurrentLinkedQueue<>(al));
        c("linkedBlockingDeque", new java.util.concurrent.LinkedBlockingDeque<>(al));
        c("treeMapDescKeys", new TreeMap<>(hm).descendingMap().keySet());
        c("subMapKeys", new TreeMap<>(hm).headMap("zzz").keySet());
        c("synchMapKeys", Collections.synchronizedMap(hm).keySet());
        c("synchSet", Collections.synchronizedSet(new HashSet<>(al)));
        c("singletonSet", Collections.singleton("x"));
        c("emptySet", Collections.emptySet());
        c("emptyMapKeys", Collections.<String,String>emptyMap().keySet());

        c("hashSet", new HashSet<>(al));
        c("linkedHashSet", new LinkedHashSet<>(al));
        c("treeSet", new TreeSet<>(al));
        c("unmodSet", Collections.unmodifiableSet(new HashSet<>(al)));
        c("unmodMapKeys", Collections.unmodifiableMap(hm).keySet());
        c("unmodMapValues", Collections.unmodifiableMap(hm).values());
        c("synchList", Collections.synchronizedList(al));
        c("emptyList", Collections.emptyList());
        c("singletonList", Collections.singletonList("x"));
        c("listOf", List.of("a","b"));
        c("setOf", Set.of("a"));
        c("mapOfKeys", Map.of("a","b").keySet());
        c("arrayDeque", new ArrayDeque<>(al));
        c("priorityQueue", new PriorityQueue<>(al));
        c("vector", new Vector<>(al));
        c("stack", new Stack<String>());
        c("enumSetLike", EnumSet.allOf(java.time.DayOfWeek.class));
    }
}
