import org.apache.catalina.util.ParameterMap;
import java.util.*;
public class PmTest2 {
    static void chk(String n, Runnable r){
        try { r.run(); System.out.println("FAIL "+n+" (no exception)"); }
        catch(UnsupportedOperationException e){ System.out.println("OK   "+n); }
        catch(Throwable t){ System.out.println("WRONG "+n+": "+t.getClass().getName()); }
    }
    public static void main(String[] a){
        ParameterMap<String,String> pm = new ParameterMap<>();
        pm.put("param1","v1"); pm.put("param2","v2");
        pm.setLocked(true);
        Set<String> ks = pm.keySet();
        chk("add", () -> ks.add("p4"));
        chk("remove", () -> ks.remove("param2"));
        chk("removeIf", () -> ks.removeIf(x -> "param2".equals(x)));
        chk("removeAll", () -> ks.removeAll(Arrays.asList("param1","param2")));
        chk("retainAll", () -> ks.retainAll(Collections.emptyList()));
        chk("clear", () -> ks.clear());
        System.out.println("--- entrySet ---");
        Set<Map.Entry<String,String>> es = pm.entrySet();
        chk("es.removeIf", () -> es.removeIf(x -> true));
        chk("es.removeAll", () -> es.removeAll(Collections.emptyList()));
        chk("es.retainAll", () -> es.retainAll(Collections.emptyList()));
        chk("es.clear", () -> es.clear());
        System.out.println("--- values ---");
        Collection<String> vs = pm.values();
        chk("vs.removeIf", () -> vs.removeIf(x -> true));
        chk("vs.removeAll", () -> vs.removeAll(Collections.emptyList()));
        chk("vs.retainAll", () -> vs.retainAll(Collections.emptyList()));
        chk("vs.clear", () -> vs.clear());
    }
}
