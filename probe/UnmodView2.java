import java.util.*;
public class UnmodView2 {
    static void chk(String n, Runnable r){
        try { r.run(); System.out.println("FAIL "+n+" (no exception)"); }
        catch(UnsupportedOperationException e){ System.out.println("OK   "+n); }
        catch(Throwable t){ System.out.println("WRONG "+n+": "+t.getClass().getName()); }
    }
    public static void main(String[] a){
        LinkedHashMap<String,String> m = new LinkedHashMap<>(); m.put("param1","v"); m.put("param2","v");
        Map<String,String> um = Collections.unmodifiableMap(m);
        Set<String> ks = um.keySet();
        chk("keySet.add", () -> ks.add("param4"));
        chk("keySet.remove", () -> ks.remove("param2"));
        chk("keySet.removeIf", () -> ks.removeIf(x -> "param2".equals(x)));
        chk("keySet.clear", () -> ks.clear());
        Set<Map.Entry<String,String>> es = um.entrySet();
        chk("entrySet.removeIf", () -> es.removeIf(x -> true));
        chk("entrySet.clear", () -> es.clear());
        chk("map.remove", () -> um.remove("param1"));
    }
}
