import java.util.*;
import java.lang.reflect.*;

public class SetProbe2 {
    static class Plain { public String toString() { return "P@" + Integer.toHexString(hashCode()); } }

    static void dump(String label, Set<Object> s) throws Exception {
        Plain p = new Plain();
        System.out.println(label + " add=" + s.add(p) + " size=" + s.size());
        Field f = HashSet.class.getDeclaredField("map");
        f.setAccessible(true);
        Object m = f.get(s);
        System.out.println(label + "   backing map class=" + (m == null ? "null" : m.getClass().getName())
                + " mapSize=" + (m == null ? "-" : ((Map<?, ?>) m).size()));
        if (m != null) {
            for (Map.Entry<?, ?> e : ((Map<?, ?>) m).entrySet()) {
                Object v = e.getValue();
                System.out.println(label + "   entry key=" + e.getKey() + " value=" + v
                        + " valueClass=" + (v == null ? "null" : v.getClass().getName()));
            }
        }
        System.out.println(label + " remove=" + s.remove(p) + " sizeAfter=" + s.size());
        // and via the raw backing map
        Plain q = new Plain();
        s.add(q);
        if (m != null) {
            Object old = ((Map<Object, Object>) m).remove(q);
            System.out.println(label + "   backingMap.remove -> " + old
                    + " class=" + (old == null ? "null" : old.getClass().getName()));
        }
    }

    public static void main(String[] args) throws Exception {
        dump("HashSet      ", new HashSet<>());
        dump("LinkedHashSet", new LinkedHashSet<>());
    }
}
