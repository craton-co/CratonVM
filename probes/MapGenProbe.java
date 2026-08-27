import java.lang.reflect.Field;
import java.util.HashMap;
import java.util.Set;

/**
 * The keySet-view rebuild elision (`perf/lazy-map-views-*`) skips a resync when
 * the source map's `modCount` has not moved since the view's backing was built.
 * ecj's `StackMapFrameCodeStream.getFramePositions` is `int[] a = new
 * int[set.size()]; for (Object k : set) a[n++] = ...;` — so a view whose
 * `size()` is stale-low while its iterator is fresh throws
 * ArrayIndexOutOfBoundsException with index == length. This measures both
 * halves directly, plus the `modCount` the guard reads.
 */
public class MapGenProbe {
    public static void main(String[] args) throws Exception {
        HashMap<Integer, Object> m = new HashMap<>();
        Set<Integer> ks = m.keySet();
        Field f = HashMap.class.getDeclaredField("modCount");
        f.setAccessible(true);

        report("empty", m, ks, f);
        for (int i = 0; i < 3; i++) {
            m.put(1000 + i * 7, new Object());
        }
        report("after 3 puts", m, ks, f);
        for (int i = 3; i < 6; i++) {
            m.put(1000 + i * 7, new Object());
        }
        report("after 6 puts", m, ks, f);
        m.remove(1000);
        report("after remove", m, ks, f);
        m.clear();
        report("after clear", m, ks, f);

        // The exact ecj shape.
        HashMap<Integer, Object> m2 = new HashMap<>();
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 3; i++) {
                Integer k = Integer.valueOf(round * 100 + i);
                if (m2.get(k) == null) {
                    m2.put(k, new Object());
                }
            }
            Set<Integer> s = m2.keySet();
            int size = s.size();
            int[] positions = new int[size];
            int n = 0;
            try {
                for (Integer pos : s) {
                    positions[n++] = pos.intValue();
                }
                System.out.println("ecj-shape round " + round + ": size=" + size + " iterated=" + n);
            } catch (ArrayIndexOutOfBoundsException e) {
                System.out.println("ecj-shape round " + round + ": size=" + size + " AIOOBE " + e.getMessage());
            }
        }
    }

    private static void report(String what, HashMap<Integer, Object> m, Set<Integer> ks, Field f)
            throws Exception {
        int iterated = 0;
        for (Object o : ks) {
            iterated++;
        }
        System.out.println(what + ": map.size=" + m.size() + " keySet.size=" + ks.size()
                + " iterated=" + iterated + " modCount=" + f.get(m));
    }
}
