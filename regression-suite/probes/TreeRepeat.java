import java.util.Iterator;
import java.util.Map;
import java.util.TreeMap;

/**
 * Is it the OPERATION or the POSITION?
 *
 * The phase bisect put the first failure at phase 6, `headMap(k, false)`. But
 * the guard says `last_publish_at_collection=1 collections_now=2`, i.e. the
 * failure lands on the SECOND collection — and phase 6 is simply where the
 * second collection falls in that sequence. `H0-8`'s rule applies exactly:
 * anything latched per process is confounded with position, and a collection
 * counter is the purest per-process latch there is.
 *
 * So walk ONE view kind, N times, over one map. If the failure lands on the
 * same ordinal walk regardless of which view it is, the operation identity is
 * irrelevant and the answer is "the second collection", not "headMap(k,false)".
 *
 * Usage: java TreeRepeat &lt;view&gt; &lt;count&gt;
 *   view: head | tail | sub | headF | entry
 */
public class TreeRepeat {

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
    }

    static final class V {
        final long tag;

        V(long tag) {
            this.tag = tag;
        }
    }

    static long walk(String what, Map<K, V> view) {
        long seen = 0;
        for (Iterator<Map.Entry<K, V>> it = view.entrySet().iterator(); it.hasNext();) {
            Map.Entry<K, V> e = it.next();
            Object ko = e.getKey();
            if (!(ko instanceof K)) {
                throw new AssertionError(what + ": key is a "
                        + (ko == null ? "null" : ko.getClass().getName()));
            }
            seen++;
        }
        return seen;
    }

    public static void main(String[] a) {
        String view = a.length > 0 ? a[0] : "head";
        int count = a.length > 1 ? Integer.parseInt(a[1]) : 8;
        long lo = ENTRIES / 4, hi = (3 * ENTRIES) / 4;

        TreeMap<K, V> m = new TreeMap<K, V>();
        for (int i = 0; i < ENTRIES; i++) {
            long k = (i * 7919) % ENTRIES;
            m.put(new K(k), new V(k * 3 + 1));
        }

        for (int n = 1; n <= count; n++) {
            Map<K, V> v;
            switch (view) {
                case "head":  v = m.headMap(new K(hi)); break;
                case "tail":  v = m.tailMap(new K(lo)); break;
                case "sub":   v = m.subMap(new K(lo), new K(hi)); break;
                case "headF": v = m.headMap(new K(hi), false); break;
                case "entry": v = m; break;
                default: System.out.println("unknown view " + view); return;
            }
            long seen;
            try {
                seen = walk(view + "#" + n, v);
            } catch (Throwable t) {
                System.out.println("FAILAT walk=" + n + " view=" + view
                        + " ex=" + t.getClass().getName());
                throw t;
            }
            System.out.println("walk " + n + " " + view + " seen=" + seen);
        }
        System.out.println("OK view=" + view + " walks=" + count);
    }
}
