import java.util.*;
import java.lang.reflect.Field;
/** Does the REAL HashMap.size field track, or only the native's counter? */
public class MapSizeField {
    static Field F;
    static int raw(Map<?,?> m) throws Exception {
        if (F == null) { F = HashMap.class.getDeclaredField("size"); F.setAccessible(true); }
        return F.getInt(m);
    }
    static void row(String n, Object a, Object b) { System.out.println("CK " + n + " nativeSize=" + a + " rawField=" + b); }
    public static void main(String[] x) throws Exception {
        Map<String,String> m = new LinkedHashMap<>();
        row("empty", m.size(), raw(m));
        m.put("a","1");
        row("after put a", m.size(), raw(m));
        m.put("b","2"); m.put("c","3");
        row("after put b,c", m.size(), raw(m));
        m.remove("a");
        row("after map.remove(a)", m.size(), raw(m));
        Collection<String> v = m.values();
        v.remove("2");
        row("after view.remove(2)", m.size(), raw(m));
        Iterator<String> it = m.keySet().iterator(); it.next(); it.remove();
        row("after keySet it.remove", m.size(), raw(m));
        m.clear();
        row("after clear", m.size(), raw(m));
        Map<String,String> h = new HashMap<>();
        h.put("k","v"); h.put("j","w");
        row("plain HashMap 2 puts", h.size(), raw(h));
        h.values().remove("v");
        row("plain HashMap view.remove", h.size(), raw(h));
    }
}
