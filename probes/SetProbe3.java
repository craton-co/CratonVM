import java.util.*;
import java.lang.reflect.*;

public class SetProbe3 {
    static class Plain { public String toString() { return "P@" + Integer.toHexString(hashCode()); } }

    static String cls(Object o) { return o == null ? "null" : o.getClass().getName(); }

    @SuppressWarnings("unchecked")
    static void dump(String label, Set<Object> s) throws Exception {
        Field f = HashSet.class.getDeclaredField("map");
        f.setAccessible(true);

        Plain a = new Plain();
        s.add(a);
        Map<Object, Object> m = (Map<Object, Object>) f.get(s);
        System.out.println(label + " backing=" + cls(m) + " size=" + (m == null ? -1 : m.size()));
        Object viaMap = (m == null) ? null : m.remove(a);
        System.out.println(label + " backingMap.remove(a) -> " + viaMap + " (" + cls(viaMap) + ")"
                + " mapSizeAfter=" + (m == null ? -1 : m.size()));

        Plain b = new Plain();
        s.add(b);
        System.out.println(label + " set.remove(b) -> " + s.remove(b) + " setSizeAfter=" + s.size());

        Plain c = new Plain();
        s.add(c);
        System.out.println(label + " AbstractCollection-style via iterator.remove: ");
        Iterator<Object> it = s.iterator();
        int n = 0;
        while (it.hasNext()) { it.next(); it.remove(); n++; }
        System.out.println(label + "   iterated=" + n + " sizeAfter=" + s.size());
    }

    public static void main(String[] args) throws Exception {
        dump("HashSet      ", new HashSet<>());
        dump("LinkedHashSet", new LinkedHashSet<>());
    }
}
