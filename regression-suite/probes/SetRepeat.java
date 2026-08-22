import java.util.Iterator;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;

/**
 * The sites `TreeRepeat` does not reach.
 *
 * No TreeMap range view fails on the generational collector (64 walks, 2/2,
 * all five kinds) — yet the full `RTreeRangeGc` vector DOES fail there. So the
 * vector's generational failure comes from a site the map-range probe never
 * touches. The candidates are the TreeSet range views, `keySet`, `toString`,
 * and holding a map and a set live together.
 *
 * Usage: java SetRepeat &lt;case&gt; &lt;count&gt;
 *   headSet tailSet subSet keySet toString both
 */
public class SetRepeat {

    static final int ENTRIES = 400;
    static volatile Object sink;

    static void churn() {
        Object[] junk = new Object[32];
        for (int i = 0; i < junk.length; i++) {
            junk[i] = new byte[256];
        }
        sink = junk;
    }

    static final class K implements Comparable<K> {
        final long key;
        final long payload;

        K(long key) {
            this.key = key;
            this.payload = key * 3 + 1;
        }

        @Override
        public int compareTo(K o) {
            churn();
            return Long.compare(this.key, o.key);
        }

        @Override
        public String toString() {
            return "K(" + key + "," + payload + ")";
        }
    }

    static final class V {
        final long tag;

        V(long tag) {
            this.tag = tag;
        }
    }

    static long walkSet(String what, Iterable<K> view) {
        long seen = 0;
        for (Iterator<K> it = view.iterator(); it.hasNext();) {
            Object ko = it.next();
            if (!(ko instanceof K)) {
                throw new AssertionError(what + ": element is a "
                        + (ko == null ? "null" : ko.getClass().getName()));
            }
            K k = (K) ko;
            if (k.payload != k.key * 3 + 1) {
                throw new AssertionError(what + ": corrupted " + k.key);
            }
            seen++;
        }
        return seen;
    }

    public static void main(String[] a) {
        String c = a.length > 0 ? a[0] : "subSet";
        int count = a.length > 1 ? Integer.parseInt(a[1]) : 64;
        long lo = ENTRIES / 4, hi = (3 * ENTRIES) / 4;

        TreeSet<K> s = new TreeSet<K>();
        for (int i = 0; i < ENTRIES; i++) {
            s.add(new K((i * 7919) % ENTRIES));
        }
        TreeMap<K, V> m = new TreeMap<K, V>();
        for (int i = 0; i < ENTRIES; i++) {
            long k = (i * 7919) % ENTRIES;
            m.put(new K(k), new V(k * 3 + 1));
        }

        for (int n = 1; n <= count; n++) {
            try {
                switch (c) {
                    case "headSet":  walkSet(c, s.headSet(new K(hi))); break;
                    case "tailSet":  walkSet(c, s.tailSet(new K(lo))); break;
                    case "subSet":   walkSet(c, s.subSet(new K(lo), new K(hi))); break;
                    case "keySet":   walkSet(c, m.keySet()); break;
                    case "toString": if (m.toString().length() <= ENTRIES) {
                                         throw new AssertionError("toString truncated");
                                     }
                                     break;
                    case "both":     walkSet("both.sub", s.subSet(new K(lo), new K(hi)));
                                     for (Iterator<Map.Entry<K, V>> it =
                                              m.subMap(new K(lo), new K(hi)).entrySet().iterator();
                                          it.hasNext();) {
                                         Map.Entry<K, V> e = it.next();
                                         if (!(e.getKey() instanceof K)) {
                                             throw new AssertionError("both.map: bad key");
                                         }
                                     }
                                     break;
                    default: System.out.println("unknown case " + c); return;
                }
            } catch (Throwable t) {
                System.out.println("FAILAT walk=" + n + " case=" + c
                        + " ex=" + t.getClass().getName());
                throw t;
            }
        }
        System.out.println("OK case=" + c + " walks=" + count);
    }
}
