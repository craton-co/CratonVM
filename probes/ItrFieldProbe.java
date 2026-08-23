import java.lang.reflect.Field;
import java.util.*;

/**
 * What does a values-shaped view iterator actually look like, and does its
 * declared `expectedModCount` hold anything?
 *
 * Run with --add-opens java.base/java.util=ALL-UNNAMED
 */
public class ItrFieldProbe {

    static void dump(String label, Object itr) {
        Class<?> c = itr.getClass();
        StringBuilder sb = new StringBuilder();
        for (Class<?> k = c; k != null && k != Object.class; k = k.getSuperclass()) {
            for (Field f : k.getDeclaredFields()) {
                String v;
                try { f.setAccessible(true); Object o = f.get(itr); v = String.valueOf(o); }
                catch (Throwable t) { v = "<" + t.getClass().getSimpleName() + ">"; }
                if (v.length() > 26) { v = v.substring(0, 26) + "…"; }
                sb.append(' ').append(k.getSimpleName()).append('.').append(f.getName())
                  .append('=').append(v);
            }
        }
        System.out.printf("ITR %-26s class=%-38s%s%n", label, c.getName(), sb);
    }

    public static void main(String[] args) {
        Map<String, String> tm = new TreeMap<>();
        tm.put("a", "1"); tm.put("b", "2");
        dump("TreeMap.entrySet", tm.entrySet().iterator());
        dump("TreeMap.values", tm.values().iterator());
        dump("TreeMap.keySet", tm.keySet().iterator());

        Map<String, String> hm = new HashMap<>();
        hm.put("a", "1"); hm.put("b", "2");
        dump("HashMap.values", hm.values().iterator());
        dump("HashMap.entrySet", hm.entrySet().iterator());

        // Is the entrySet view even marked as a view (does it write through)?
        Set<Map.Entry<String, String>> es = tm.entrySet();
        System.out.println("ES class=" + es.getClass().getName());
        Iterator<Map.Entry<String, String>> it = es.iterator();
        it.next();
        it.remove();
        System.out.println("after it.remove(): tm=" + tm + " (HotSpot: {b=2})");
    }
}
