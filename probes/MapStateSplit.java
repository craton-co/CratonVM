import java.util.*;
/** Where does HashMap's size come from after a write through the values view? */
public class MapStateSplit {
    static void row(String n, Object v) { System.out.println("CK " + n + " = " + v); }
    public static void main(String[] a) {
        Map<String,String> m = new LinkedHashMap<>();
        m.put("a","1"); m.put("b","2"); m.put("c","3");
        Collection<String> v = m.values();
        row("size.before", m.size());
        row("view.size.before", v.size());
        boolean removed = v.remove("2");
        row("view.remove(2)", removed);
        row("map.containsKey(b)", m.containsKey("b"));
        row("map.size.after", m.size());
        row("view.size.after", v.size());
        row("map.keySet.size", m.keySet().size());
        row("map.entrySet.size", m.entrySet().size());
        row("map.toString", m.toString());
        row("view.toString", v.toString());
        int n = 0; for (String s : m.keySet()) n++;
        row("keySet.iterated", n);
        int e = 0; for (Map.Entry<String,String> x : m.entrySet()) e++;
        row("entrySet.iterated", e);
        int w = 0; for (String s : v) w++;
        row("values.iterated", w);
        row("map.isEmpty", m.isEmpty());
        row("map.get(a)", m.get("a"));
        row("map.get(c)", m.get("c"));
    }
}
