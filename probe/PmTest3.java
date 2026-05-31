import org.apache.catalina.util.ParameterMap;
import java.util.*;
public class PmTest3 {
    static void chk(String n, Runnable r){
        try { r.run(); System.out.println("FAIL "+n+" (no exception)"); }
        catch(UnsupportedOperationException e){ System.out.println("OK   "+n); }
        catch(Throwable t){ System.out.println("WRONG "+n+": "+t.getClass().getName()); }
    }
    public static void main(String[] a){
        ParameterMap<String,String[]> pm = new ParameterMap<>();
        pm.put("param1", new String[]{"1"});
        pm.put("param2", new String[]{"2"});
        pm.put("param3", new String[]{"3"});
        // unlocked keySet usage (mirrors setUp)
        Set<String> ks0 = pm.keySet();
        System.out.println("unlocked keySet class="+ks0.getClass().getName()+" contains param1="+ks0.contains("param1"));
        pm.put("param2", new String[]{"2u"});
        pm.remove("param3");
        System.out.println("after mutate, ks0 contains param3="+ks0.contains("param3"));
        // NOW lock
        pm.setLocked(true);
        Set<String> ks = pm.keySet();
        System.out.println("locked keySet class="+ks.getClass().getName());
        chk("add", () -> ks.add("p4"));
        chk("removeIf", () -> ks.removeIf(x -> "param2".equals(x)));
        chk("clear", () -> ks.clear());
    }
}
