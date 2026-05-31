import org.apache.catalina.util.ParameterMap;
import java.util.*;
public class PmTest {
    static void chk(String n, Runnable r){
        try { r.run(); System.out.println("FAIL "+n+" (no exception)"); }
        catch(UnsupportedOperationException e){ System.out.println("OK   "+n); }
        catch(Throwable t){ System.out.println("WRONG "+n+": "+t.getClass().getName()); }
    }
    public static void main(String[] a){
        ParameterMap<String,String> pm = new ParameterMap<>();
        pm.put("param1","v1"); pm.put("param2","v2");
        pm.setLocked(true);
        System.out.println("isLocked="+pm.isLocked());
        Set<String> ks = pm.keySet();
        System.out.println("keySet class="+ks.getClass().getName());
        chk("ks.add", () -> ks.add("p4"));
        chk("ks.remove", () -> ks.remove("param2"));
        chk("pm.put", () -> pm.put("p5","v"));
    }
}
