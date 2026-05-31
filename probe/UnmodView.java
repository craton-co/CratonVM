import java.util.*;
public class UnmodView {
    static void chk(String n, Runnable r){
        try { r.run(); System.out.println("FAIL "+n+" (no exception)"); }
        catch(UnsupportedOperationException e){ System.out.println("OK   "+n); }
        catch(Throwable t){ System.out.println("WRONG "+n+": "+t.getClass().getName()); }
    }
    public static void main(String[] a){
        Map<String,String> m = new HashMap<>(); m.put("k","v");
        Map<String,String> um = Collections.unmodifiableMap(m);
        chk("um.put", () -> um.put("x","y"));
        chk("um.keySet().remove", () -> um.keySet().remove("k"));
        chk("um.keySet().iterator().remove", () -> { Iterator<String> it=um.keySet().iterator(); it.next(); it.remove(); });
        chk("um.entrySet().remove", () -> um.entrySet().remove(null));
        chk("um.entrySet().iterator().remove", () -> { Iterator<Map.Entry<String,String>> it=um.entrySet().iterator(); it.next(); it.remove(); });
        chk("um.values().iterator().remove", () -> { Iterator<String> it=um.values().iterator(); it.next(); it.remove(); });
        chk("um.keySet().clear", () -> um.keySet().clear());
    }
}
