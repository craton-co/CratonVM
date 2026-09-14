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
