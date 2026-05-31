import java.util.*;

public class MapTest2 {
    static <K,V> int iterRemoveFirst(Map<K,V> map, K key) {
        Iterator<Map.Entry<K,V>> it = map.entrySet().iterator();
        while (it.hasNext()) { if (it.next().getKey().equals(key)) it.remove(); }
        return map.size();
    }
    public static void main(String[] args) {
        // 1. plain HashMap entrySet iterator.remove
        {
            HashMap<String,Integer> m = new HashMap<>();
            m.put("a",1); m.put("b",2); m.put("c",3);
            int s = iterRemoveFirst(m, "b");
            System.out.println("HashMap entrySet it.remove: size=" + s + " get(b)=" + m.get("b") + " (expect 2/null)");
        }
        // 2. LinkedHashMap keySet iterator.remove
        {
            LinkedHashMap<String,Integer> m = new LinkedHashMap<>();
            m.put("a",1); m.put("b",2); m.put("c",3);
            Iterator<String> it = m.keySet().iterator();
            while (it.hasNext()) { if (it.next().equals("b")) it.remove(); }
            System.out.println("LHM keySet it.remove: size=" + m.size() + " get(b)=" + m.get("b") + " (expect 2/null)");
        }
        // 3. LinkedHashMap values iterator.remove
        {
            LinkedHashMap<String,Integer> m = new LinkedHashMap<>();
            m.put("a",1); m.put("b",2); m.put("c",3);
            Iterator<Integer> it = m.values().iterator();
            while (it.hasNext()) { if (it.next()==2) it.remove(); }
            System.out.println("LHM values it.remove: size=" + m.size() + " get(b)=" + m.get("b") + " (expect 2/null)");
        }
        // 4. direct map.remove(key)
        {
            LinkedHashMap<String,Integer> m = new LinkedHashMap<>();
            m.put("a",1); m.put("b",2); m.put("c",3);
            m.remove("b");
            System.out.println("LHM direct remove: size=" + m.size() + " get(b)=" + m.get("b") + " (expect 2/null)");
        }
        // 5. ArrayList iterator.remove
        {
            ArrayList<String> l = new ArrayList<>(Arrays.asList("a","b","c"));
            Iterator<String> it = l.iterator();
            while (it.hasNext()) { if (it.next().equals("b")) it.remove(); }
            System.out.println("ArrayList it.remove: size=" + l.size() + " contains(b)=" + l.contains("b") + " (expect 2/false)");
        }
        // 6. HashMap with Integer keys (no String)
        {
            HashMap<Integer,Integer> m = new HashMap<>();
            m.put(1,1); m.put(2,2); m.put(3,3);
            Iterator<Map.Entry<Integer,Integer>> it = m.entrySet().iterator();
            while (it.hasNext()) { if (it.next().getKey()==2) it.remove(); }
            System.out.println("HashMap<Int> it.remove: size=" + m.size() + " get(2)=" + m.get(2) + " (expect 2/null)");
        }
        // 7. TreeMap iterator.remove
        {
            TreeMap<String,Integer> m = new TreeMap<>();
            m.put("a",1); m.put("b",2); m.put("c",3);
            Iterator<Map.Entry<String,Integer>> it = m.entrySet().iterator();
            while (it.hasNext()) { if (it.next().getKey().equals("b")) it.remove(); }
            System.out.println("TreeMap it.remove: size=" + m.size() + " get(b)=" + m.get("b") + " (expect 2/null)");
        }
    }
}
