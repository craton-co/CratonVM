import org.apache.catalina.util.ParameterMap;
import java.util.*;
public class PmLock {
    static void chk(String n, Runnable r){
        try { r.run(); System.out.println("FAIL "+n+" (no throw)"); }
        catch(UnsupportedOperationException|IllegalStateException e){ System.out.println("OK   "+n); }
        catch(Throwable t){ System.out.println("WRONG "+n+": "+t.getClass().getName()); }
    }
    public static void main(String[] a){
        ParameterMap<String,String[]> pm = new ParameterMap<>();
        pm.put("param2", new String[]{"2"});
        pm.setLocked(true);
        chk("put", () -> pm.put("p2", new String[]{"x"}));
        chk("putIfAbsent", () -> pm.putIfAbsent("p22", new String[]{"x"}));
        chk("putAll", () -> { Map<String,String[]> m=new HashMap<>(); m.put("p4",new String[]{"v"}); pm.putAll(m); });
        chk("merge", () -> pm.merge("param2", new String[]{"m"}, (x,y)->y));
        chk("remove", () -> pm.remove("param2"));
        chk("remove2", () -> pm.remove("param2", new String[]{"2"}));
        chk("replace", () -> pm.replace("param2", new String[]{"r"}));
        chk("replace3", () -> pm.replace("param2", new String[]{"2"}, new String[]{"r"}));
        chk("replaceAll", () -> pm.replaceAll((x,y)->new String[]{"z"}));
        chk("clear", () -> pm.clear());
        chk("computeIfAbsent", () -> pm.computeIfAbsent("p9", k->new String[]{"v"}));
        chk("compute", () -> pm.compute("param2", (k,v)->new String[]{"v"}));
    }
}
