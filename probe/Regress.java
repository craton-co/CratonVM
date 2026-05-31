import java.util.*;
import java.util.function.Predicate;

public class Regress {
    static int pass=0, fail=0;
    static void chk(String name, boolean cond) {
        if (cond) { pass++; } else { fail++; System.out.println("FAIL: " + name); }
    }
    public static void main(String[] args) {
        // --- normal HashSet ops unaffected ---
        HashSet<String> hs = new HashSet<>(Arrays.asList("a","b","c"));
        chk("hs.remove", hs.remove("b") && hs.size()==2 && !hs.contains("b"));
        hs.add("d"); chk("hs.add", hs.contains("d") && hs.size()==3);
        // normal HashSet iterator remove
        Iterator<String> hi = hs.iterator();
        while (hi.hasNext()) { if (hi.next().equals("a")) hi.remove(); }
        chk("hs.itr.remove", !hs.contains("a") && hs.size()==2);

        // --- normal ArrayList ops unaffected ---
        ArrayList<String> al = new ArrayList<>(Arrays.asList("x","y","z","w"));
        chk("al.remove(obj)", al.remove("y") && al.size()==3 && !al.contains("y"));
        chk("al.remove(idx)", al.remove(1).equals("z") && al.size()==2);
        Iterator<String> ai = al.iterator();
        while (ai.hasNext()) { if (ai.next().equals("x")) ai.remove(); }
        chk("al.itr.remove", !al.contains("x") && al.size()==1);
        ArrayList<Integer> al2 = new ArrayList<>(Arrays.asList(1,2,3,4,5));
        al2.removeIf(n -> n % 2 == 0);
        chk("al.removeIf", al2.equals(Arrays.asList(1,3,5)));

        // --- map keySet view write-through ---
        Map<String,Integer> m = new HashMap<>();
        m.put("a",1); m.put("b",2); m.put("c",3);
        m.keySet().remove("a");
        chk("keySet.remove", !m.containsKey("a") && m.size()==2);
        Iterator<String> ks = m.keySet().iterator();
        while (ks.hasNext()) { if (ks.next().equals("b")) ks.remove(); }
        chk("keySet.itr.remove", !m.containsKey("b") && m.size()==1);
        m.keySet().removeIf(k -> k.equals("c"));
        chk("keySet.removeIf", m.isEmpty());

        // --- entrySet view write-through ---
        Map<String,Integer> m2 = new LinkedHashMap<>();
        m2.put("a",1); m2.put("b",2); m2.put("c",3);
        Iterator<Map.Entry<String,Integer>> es = m2.entrySet().iterator();
        while (es.hasNext()) { if (es.next().getKey().equals("b")) es.remove(); }
        chk("entrySet.itr.remove", !m2.containsKey("b") && m2.size()==2);
        m2.entrySet().removeIf(e -> e.getValue()==3);
        chk("entrySet.removeIf", !m2.containsKey("c") && m2.size()==1 && m2.containsKey("a"));

        // --- values view write-through ---
        Map<String,Integer> m3 = new HashMap<>();
        m3.put("a",10); m3.put("b",20); m3.put("c",30);
        Iterator<Integer> vs = m3.values().iterator();
        while (vs.hasNext()) { if (vs.next()==20) vs.remove(); }
        chk("values.itr.remove", !m3.containsValue(20) && m3.size()==2 && m3.get("a")==10);
        m3.values().remove(Integer.valueOf(10));
        chk("values.remove", !m3.containsKey("a") && m3.size()==1);

        // --- read-only view iteration still works ---
        Map<String,Integer> m4 = new HashMap<>();
        m4.put("p",1); m4.put("q",2);
        int sum=0; for (int v : m4.values()) sum+=v;
        chk("values.sum", sum==3);
        Set<String> keys = new HashSet<>(m4.keySet());
        chk("keySet.copy", keys.size()==2 && keys.contains("p") && keys.contains("q"));
        chk("keySet.size live", m4.keySet().size()==2);

        // --- clear via keySet ---
        Map<String,Integer> m5 = new HashMap<>();
        m5.put("a",1); m5.put("b",2);
        m5.keySet().clear();
        chk("keySet.clear", m5.isEmpty());

        System.out.println("PASS=" + pass + " FAIL=" + fail);
    }
}
