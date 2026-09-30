import java.util.*;
import java.util.concurrent.*;

/** Collection CONTRACTS — fail-fast, view aliasing, equals/hashCode, entry
 *  mutation. native-collections implements most of these, and the vectors
 *  exercise values far more than contracts. Deterministic, ASCII. */
public class Sweep5CollectionContracts {
    static void t(String l, Call c) {
        try { System.out.println("C " + l + " = " + c.run()); }
        catch (Throwable x) { System.out.println("C " + l + " = THREW " + x.getClass().getName()); }
    }
    interface Call { Object run() throws Exception; }
    public static void main(String[] a) {
        // fail-fast iterators
        t("al_failfast", () -> { List<Integer> l = new ArrayList<>(List.of(1,2,3));
            Iterator<Integer> i = l.iterator(); i.next(); l.add(4); return i.next(); });
        t("hm_failfast", () -> { Map<String,Integer> m = new HashMap<>(Map.of("a",1));
            Iterator<String> i = m.keySet().iterator(); m.put("b",2); return i.next(); });
        t("tm_failfast", () -> { TreeMap<Integer,Integer> m = new TreeMap<>(Map.of(1,1));
            Iterator<Integer> i = m.keySet().iterator(); m.put(2,2); return i.next(); });
        t("hs_failfast", () -> { Set<Integer> s = new HashSet<>(Set.of(1));
            Iterator<Integer> i = s.iterator(); s.add(9); return i.next(); });
        t("chm_no_failfast", () -> { Map<String,Integer> m = new ConcurrentHashMap<>(Map.of("a",1));
            Iterator<String> i = m.keySet().iterator(); m.put("b",2); i.next(); return "no CME"; });
        // G67-1 N2 -- the doors §4 asked for beyond keySet(), plus the rest of
        // the family the same snapshot-iterator species could have reached.
        t("hm_values_failfast", () -> { Map<String,Integer> m = new HashMap<>(Map.of("a",1));
            Iterator<Integer> i = m.values().iterator(); m.put("b",2); return i.next(); });
        t("hm_entryset_failfast", () -> { Map<String,Integer> m = new HashMap<>(Map.of("a",1));
            Iterator<Map.Entry<String,Integer>> i = m.entrySet().iterator(); m.put("b",2); return i.next(); });
        t("lhm_keyset_failfast", () -> { Map<String,Integer> m = new LinkedHashMap<>(Map.of("a",1));
            Iterator<String> i = m.keySet().iterator(); m.put("b",2); return i.next(); });
        t("tm_values_failfast", () -> { TreeMap<Integer,Integer> m = new TreeMap<>(Map.of(1,1));
            Iterator<Integer> i = m.values().iterator(); m.put(2,2); return i.next(); });
        t("tm_entryset_failfast", () -> { TreeMap<Integer,Integer> m = new TreeMap<>(Map.of(1,1));
            Iterator<Map.Entry<Integer,Integer>> i = m.entrySet().iterator(); m.put(2,2); return i.next(); });
        t("lhs_failfast", () -> { Set<Integer> s = new LinkedHashSet<>(Set.of(1));
            Iterator<Integer> i = s.iterator(); s.add(9); return i.next(); });
        t("ts_failfast", () -> { Set<Integer> s = new TreeSet<>(Set.of(1));
            Iterator<Integer> i = s.iterator(); s.add(9); return i.next(); });
        t("ht_keyset_failfast", () -> { Hashtable<String,Integer> m = new Hashtable<>(Map.of("a",1));
            Iterator<String> i = m.keySet().iterator(); m.put("b",2); return i.next(); });
        // Other snapshot-shaped iterators the same species could have reached.
        // Vector/Stack/PriorityQueue/ArrayDeque/SetFromMap/synchronizedList
        // must be fail-fast; CopyOnWriteArrayList/LinkedBlockingQueue must NOT
        // be -- both are weakly-consistent BY SPECIFICATION, and asserting the
        // negative here is the same discipline `chm_no_failfast` already uses.
        t("vector_failfast", () -> { Vector<Integer> v = new Vector<>(List.of(1,2));
            Iterator<Integer> i = v.iterator(); v.add(9); return i.next(); });
        t("stack_failfast", () -> { Stack<Integer> s = new Stack<>(); s.push(1); s.push(2);
            Iterator<Integer> i = s.iterator(); s.push(9); return i.next(); });
        t("pq_failfast", () -> { PriorityQueue<Integer> q = new PriorityQueue<>(List.of(3,1,2));
            Iterator<Integer> i = q.iterator(); q.add(9); return i.next(); });
        t("ad_failfast", () -> { ArrayDeque<Integer> d = new ArrayDeque<>(List.of(1,2));
            Iterator<Integer> i = d.iterator(); d.add(9); return i.next(); });
        t("setfrommap_failfast", () -> { Set<Integer> s = Collections.newSetFromMap(new IdentityHashMap<>());
            s.add(1); Iterator<Integer> i = s.iterator(); s.add(2); return i.next(); });
        t("synclist_failfast", () -> { List<Integer> l = Collections.synchronizedList(new ArrayList<>(List.of(1,2)));
            Iterator<Integer> i = l.iterator(); l.add(9); return i.next(); });
        t("cow_no_failfast", () -> { CopyOnWriteArrayList<Integer> l = new CopyOnWriteArrayList<>(List.of(1,2));
            Iterator<Integer> i = l.iterator(); l.add(9); i.next(); i.next(); return i.hasNext() ? "?" : "no CME, snapshot exhausted at 2"; });
        t("lbq_no_failfast", () -> { LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>(List.of(1,2));
            Iterator<Integer> i = q.iterator(); q.add(9); i.next(); i.next(); return i.hasNext() ? i.next() : "no CME, sees the write"; });
        // iterator misuse
        t("it_remove_twice", () -> { List<Integer> l = new ArrayList<>(List.of(1,2));
            Iterator<Integer> i = l.iterator(); i.next(); i.remove(); i.remove(); return "no ISE"; });
        t("it_remove_before_next", () -> { Iterator<Integer> i = new ArrayList<>(List.of(1)).iterator();
            i.remove(); return "no ISE"; });
        t("it_next_at_end", () -> { Iterator<Integer> i = new ArrayList<Integer>().iterator();
            return i.next(); });
        // view aliasing
        t("sublist_write_through", () -> { List<Integer> l = new ArrayList<>(List.of(1,2,3,4));
            l.subList(1,3).set(0, 9); return l.toString(); });
        t("sublist_struct_invalidates", () -> { List<Integer> l = new ArrayList<>(List.of(1,2,3,4));
            List<Integer> s = l.subList(1,3); l.add(5); return s.get(0); });
        t("keyset_remove_writes_through", () -> { Map<String,Integer> m = new HashMap<>(new HashMap<>(Map.of("a",1,"b",2)));
            m.keySet().remove("a"); return m.toString(); });
        t("values_remove_writes_through", () -> { Map<String,Integer> m = new HashMap<>(Map.of("a",1));
            m.values().remove(1); return m.size(); });
        t("entry_setValue", () -> { Map<String,Integer> m = new HashMap<>(Map.of("a",1));
            for (Map.Entry<String,Integer> e : m.entrySet()) e.setValue(7); return m.toString(); });
        t("keyset_add_unsupported", () -> { Map<String,Integer> m = new HashMap<>(Map.of("a",1));
            m.keySet().add("z"); return "no UOE"; });
        // equals / hashCode contracts
        t("list_equals_across_impls", () -> new ArrayList<>(List.of(1,2)).equals(new LinkedList<>(List.of(1,2))));
        t("set_equals_across_impls", () -> new HashSet<>(Set.of(1,2)).equals(new TreeSet<>(Set.of(1,2))));
        t("map_equals_across_impls", () -> new HashMap<>(Map.of("a",1)).equals(new TreeMap<>(Map.of("a",1))));
        t("list_hashCode", () -> List.of(1,2,3).hashCode());
        t("set_hashCode", () -> Set.of(1,2,3).hashCode());
        t("map_hashCode", () -> Map.of("a",1).hashCode());
        t("empty_equals", () -> List.of().equals(new ArrayList<>()));
        // sorted-map navigation and submap bounds
        t("submap_bounds", () -> { TreeMap<Integer,String> m = new TreeMap<>();
            for (int i=0;i<5;i++) m.put(i, "v"+i);
            return m.subMap(1,true,3,false).toString(); });
        t("submap_out_of_range_put", () -> { TreeMap<Integer,String> m = new TreeMap<>(Map.of(1,"a",5,"b"));
            m.subMap(1,3).put(9,"x"); return "no IAE"; });
        t("descending", () -> new TreeMap<>(Map.of(1,"a",2,"b")).descendingMap().toString());
        // null policy
        t("hashmap_null_key", () -> { Map<String,Integer> m = new HashMap<>(); m.put(null,1); return m.get(null); });
        t("treemap_null_key", () -> { TreeMap<String,Integer> m = new TreeMap<>(); m.put(null,1); return "no NPE"; });
        t("listof_null", () -> List.of((Object) null));
        t("arraylist_null_ok", () -> { List<Object> l = new ArrayList<>(); l.add(null); return l.toString(); });
        System.out.println("C done = 1");
    }
}
