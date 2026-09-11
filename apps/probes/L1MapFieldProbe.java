import java.lang.reflect.Field;
import java.util.*;

/** L1 §10 item 3 — WHICH field is wrong when a `java/util/HashMap` retirement
 *  empties the views.
 *
 *  Arming `java/util/HashMap` takes `MapViewsShadowSweep` from 0 diffs to 4
 *  and `ItrCarrierCensus`'s HashSet row from `walk=[a,b,c,]` to
 *  `walk=[] failFast=NullPointerException`. Real `HashMap$HashIterator` opens
 *
 *      expectedModCount = modCount;
 *      Node<K,V>[] t = table;
 *      if (t != null && size > 0) { do {} while (index < t.length && ...); }
 *
 *  so an EMPTY walk is `table == null` or `size <= 0`, and those are different
 *  defects with different fixes. This reads the four fields the real iterator
 *  reads, on four receivers built four ways, and prints SHAPES only — the
 *  array's length and component type, never its contents or identity.
 */
public final class L1MapFieldProbe {
    static int rows = 0;

    static void dump(String tag, Object map) {
        rows++;
        StringBuilder sb = new StringBuilder(tag).append(" |");
        try {
            Class<?> hm = Class.forName("java.util.HashMap");
            sb.append("cls=").append(map.getClass().getName());
            for (String n : new String[] { "table", "size", "modCount", "threshold", "loadFactor" }) {
                Object v;
                try {
                    Field f = fieldOf(map.getClass(), n);
                    if (f == null) { sb.append(' ').append(n).append("=<no-field>"); continue; }
                    f.setAccessible(true);
                    v = f.get(map);
                } catch (Throwable e) {
                    sb.append(' ').append(n).append("=<").append(e.getClass().getSimpleName()).append('>');
                    continue;
                }
                if (v == null) { sb.append(' ').append(n).append("=null"); }
                else if (v.getClass().isArray()) {
                    sb.append(' ').append(n).append("=[").append(java.lang.reflect.Array.getLength(v))
                      .append("]").append(v.getClass().getComponentType().getName());
                } else { sb.append(' ').append(n).append('=').append(v); }
            }
        } catch (Throwable e) {
            sb.append("THREW ").append(e.getClass().getName());
        }
        System.out.println(sb.append('|'));
    }

    static Field fieldOf(Class<?> c, String n) {
        for (Class<?> k = c; k != null; k = k.getSuperclass()) {
            try { return k.getDeclaredField(n); } catch (NoSuchFieldException ignored) { }
        }
        return null;
    }

    static void walk(String tag, Map<String, String> m) {
        rows++;
        try {
            List<String> keys = new ArrayList<>();
            for (String k : m.keySet()) { keys.add(k); }
            Collections.sort(keys);
            List<String> ewalk = new ArrayList<>();
            for (Map.Entry<String, String> e : m.entrySet()) { ewalk.add(e.getKey() + "=" + e.getValue()); }
            Collections.sort(ewalk);
            List<String> vwalk = new ArrayList<>(m.values());
            Collections.sort(vwalk);
            System.out.println(tag + " |keySet.size=" + m.keySet().size()
                    + " walk=" + keys
                    + " entrySet.size=" + m.entrySet().size()
                    + " entryWalk=" + ewalk
                    + " values.size=" + m.values().size()
                    + " valueWalk=" + vwalk
                    + " eToArray=" + m.entrySet().toArray().length
                    + " kToArray=" + m.keySet().toArray().length
                    + " vToArray=" + m.values().toArray().length
                    + " map.size=" + m.size() + "|");
        } catch (Throwable e) {
            System.out.println(tag + " |THREW " + e.getClass().getName() + "|");
        }
    }

    public static void main(String[] a) {
        Map<String, String> byPut = new HashMap<>();
        byPut.put("a", "1"); byPut.put("b", "2"); byPut.put("c", "3");
        dump("A.ctor+put.fields", byPut);
        walk("A.ctor+put.views", byPut);

        Map<String, String> sized = new HashMap<>(64);
        sized.put("a", "1"); sized.put("b", "2"); sized.put("c", "3");
        dump("B.sizedCtor.fields", sized);
        walk("B.sizedCtor.views", sized);

        Map<String, String> copied = new HashMap<>(byPut);
        dump("C.copyCtor.fields", copied);
        walk("C.copyCtor.views", copied);

        Map<String, String> fresh = new HashMap<>();
        dump("D.empty.fields", fresh);
        walk("D.empty.views", fresh);

        Map<String, String> ofMap = new HashMap<>(Map.of("a", "1"));
        dump("E.fromMapOf.fields", ofMap);
        walk("E.fromMapOf.views", ofMap);

        Set<String> hs = new HashSet<>(List.of("a", "b", "c"));
        rows++;
        Object backing = null;
        try {
            Field f = fieldOf(hs.getClass(), "map");
            if (f != null) { f.setAccessible(true); backing = f.get(hs); }
        } catch (Throwable e) { backing = null; }
        System.out.println("F.hashSet.backingClass |"
                + (backing == null ? "null" : backing.getClass().getName()) + "|");
        if (backing != null) { dump("F.hashSet.backing.fields", backing); }

        Hashtable<String, String> ht = new Hashtable<>();
        ht.put("a", "1"); ht.put("b", "2");
        dump("G.hashtable.fields", ht);
        walk("G.hashtable.views", ht);

        System.out.println("rows " + rows);
        System.out.println("DONE L1MapFieldProbe");
    }

    private L1MapFieldProbe() { }
}
