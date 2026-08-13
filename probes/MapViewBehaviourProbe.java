import java.util.*;
import java.util.concurrent.ConcurrentHashMap;
import java.util.stream.Collectors;

/**
 * Every operation a `values()` / `entrySet()` view is asked to do, on every map
 * family CratonVM models, with a HotSpot oracle. One line per assertion so a
 * diff names the operation that broke.
 */
public final class MapViewBehaviourProbe {
    static StringBuilder out = new StringBuilder();

    static void p(String k, Object v) { out.append(k).append('=').append(v).append('\n'); }

    static <K, V> void exercise(String tag, Map<K, V> m, K k1, V v1, K k2, V v2, K k3, V v3) {
        m.clear();
        m.put(k1, v1); m.put(k2, v2);
        Collection<V> vals = m.values();
        Set<K> keys = m.keySet();
        Set<Map.Entry<K, V>> entries = m.entrySet();

        p(tag + ".values.size", vals.size());
        p(tag + ".values.isEmpty", vals.isEmpty());
        p(tag + ".values.contains", vals.contains(v1));
        p(tag + ".values.containsMissing", vals.contains(v3));
        p(tag + ".values.toArray.len", vals.toArray().length);
        p(tag + ".values.toArrayTyped.len", vals.toArray(new Object[0]).length);
        p(tag + ".values.stream.count", vals.stream().count());
        p(tag + ".values.sorted", vals.stream().map(String::valueOf).sorted().collect(Collectors.toList()));
        p(tag + ".keys.size", keys.size());
        p(tag + ".keys.contains", keys.contains(k1));
        p(tag + ".entries.size", entries.size());

        // live after a put
        m.put(k3, v3);
        p(tag + ".values.afterPut.size", vals.size());
        p(tag + ".values.afterPut.contains", vals.contains(v3));
        p(tag + ".keys.afterPut.size", keys.size());
        p(tag + ".entries.afterPut.size", entries.size());

        // iteration order and content
        List<String> seen = new ArrayList<>();
        for (V v : vals) { seen.add(String.valueOf(v)); }
        Collections.sort(seen);
        p(tag + ".values.iterated", seen);

        // forEach
        final int[] n = {0};
        vals.forEach(x -> n[0]++);
        p(tag + ".values.forEach.count", n[0]);

        // write-through: remove one value through the view
        boolean removed = vals.remove(v1);
        p(tag + ".values.remove", removed);
        p(tag + ".map.afterValuesRemove.size", m.size());
        p(tag + ".map.afterValuesRemove.hasK1", m.containsKey(k1));

        // write-through: iterator().remove()
        Iterator<V> it = m.values().iterator();
        if (it.hasNext()) { it.next(); it.remove(); }
        p(tag + ".map.afterIterRemove.size", m.size());

        // copy constructors read the view correctly
        p(tag + ".copyList.size", new ArrayList<>(m.values()).size());
        p(tag + ".copySet.size", new HashSet<>(m.values()).size());
        p(tag + ".copyMap.size", new LinkedHashMap<>(m).size());

        // toString shape
        p(tag + ".values.toString.len", String.valueOf(m.values()).length());

        // add is unsupported on a values view
        try { m.values().add(v1); p(tag + ".values.add", "NO-THROW"); }
        catch (UnsupportedOperationException e) { p(tag + ".values.add", "UOE"); }
        catch (RuntimeException e) { p(tag + ".values.add", e.getClass().getName()); }

        // clear through the view empties the map
        m.values().clear();
        p(tag + ".map.afterValuesClear.size", m.size());
    }

    public static void main(String[] a) {
        exercise("hm", new HashMap<String, String>(), "a", "1", "b", "2", "c", "3");
        exercise("lhm", new LinkedHashMap<String, String>(), "a", "1", "b", "2", "c", "3");
        exercise("tm", new TreeMap<String, String>(), "a", "1", "b", "2", "c", "3");
        exercise("ht", new Hashtable<String, String>(), "a", "1", "b", "2", "c", "3");
        exercise("chm", new ConcurrentHashMap<String, String>(), "a", "1", "b", "2", "c", "3");
        exercise("props", castProps(), "a", "1", "b", "2", "c", "3");
        exercise("idm", new IdentityHashMap<String, String>(), "a", "1", "b", "2", "c", "3");

        // A values view handed to code that only knows Collection.
        Map<String, Integer> m = new LinkedHashMap<>();
        for (int i = 0; i < 5; i++) { m.put("k" + i, i); }
        p("sum", m.values().stream().mapToInt(Integer::intValue).sum());
        p("max", Collections.max(m.values()));
        p("joined", m.values().stream().map(String::valueOf).collect(Collectors.joining(",")));
        p("reversedKeys", new ArrayList<>(m.keySet()).size());
        p("nestedValuesOfValues", new LinkedHashMap<>(m).values().size());
        System.out.print(out);
    }

    @SuppressWarnings("unchecked")
    static Map<String, String> castProps() { return (Map<String, String>) (Map<?, ?>) new Properties(); }
}
