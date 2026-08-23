import java.lang.reflect.Field;
import java.util.*;

/**
 * Separates the two halves of the fail-fast question, which
 * `MapModCountProbe` conflates:
 *
 *   BUMP  — does a structural edit advance the map's own `modCount` field?
 *           This is the invalidation generation any cached/lazy map view would
 *           key on (`bump_map_mod_count` in native-collections).
 *   CHECK — does the iterator consult it and throw ConcurrentModificationException?
 *
 * A view cache needs BUMP only. Fail-fast semantics need both.
 * Run with: --add-opens java.base/java.util=ALL-UNNAMED
 */
public class MapModCountProbe2 {

    static Integer readModCount(Object m) {
        for (Class<?> c = m.getClass(); c != null; c = c.getSuperclass()) {
            try {
                Field f = c.getDeclaredField("modCount");
                f.setAccessible(true);
                return f.getInt(m);
            }
            catch (NoSuchFieldException e) { /* keep walking */ }
            catch (Throwable t) { return null; }
        }
        return null;
    }

    static String bumpRow(String label, Map<String, String> m) {
        for (int i = 0; i < 4; i++) { m.put("k" + i, "v" + i); }
        Integer before = readModCount(m);
        m.put("fresh", "x");                 // structural: new key
        Integer afterPut = readModCount(m);
        m.remove("k0");                      // structural: removal
        Integer afterRemove = readModCount(m);
        m.put("fresh", "y");                 // NOT structural: value replace
        Integer afterReplace = readModCount(m);
        if (before == null) { return String.format("%-16s modCount=UNREADABLE", label); }
        return String.format("%-16s before=%d put=%d remove=%d replace=%d  bumpOnPut=%s bumpOnRemove=%s",
                label, before, afterPut, afterRemove, afterReplace,
                (afterPut > before) ? "YES" : "NO",
                (afterRemove > afterPut) ? "YES" : "NO");
    }

    public static void main(String[] args) {
        System.out.println("BUMP " + bumpRow("HashMap", new HashMap<>()));
        System.out.println("BUMP " + bumpRow("LinkedHashMap", new LinkedHashMap<>()));
        System.out.println("BUMP " + bumpRow("TreeMap", new TreeMap<>()));
        System.out.println("BUMP " + bumpRow("Hashtable", new Hashtable<>()));
    }
}
