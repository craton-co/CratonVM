import java.util.Iterator;
import java.util.Map;
import java.util.NavigableMap;
import java.util.NavigableSet;
import java.util.SortedSet;
import java.util.TreeMap;
import java.util.TreeSet;

/**
 * `RTreeRangeGc`'s SHAPE, with the phase list as an argument.
 *
 * Every probe that failed to reproduce the vector's generational failure walked
 * ONE view REPEATEDLY. The vector walks eleven views ONCE EACH with a 400-entry
 * map and a 400-entry set both live. That difference is the remaining suspect,
 * and a fixed-sequence probe cannot test it — so this takes the sequence as
 * data.
 *
 * Usage: java TreeVectorShape &lt;comma-separated phase list&gt;
 *
 *   1 build map      2 map size   3 headMap   4 tailMap   5 subMap
 *   6 headMap(k,f)   7 tailMap(k,t)           8 subMap(k,t,k,f)
 *   9 keySet        10 toString  11 build set 12 set size
 *  13 headSet       14 tailSet   15 subSet    16 subSet(k,t,k,f)
 *
 * Phases may repeat and may appear in any order, so "1,2,11,3" and "1,2,3,3,3"
 * are both legal. That is the point: it separates WHICH phase from HOW MANY
 * collections have happened, which every previous bisect here confounded.
 *
 * Prints `OK <list> checks=N` or `FAILAT phase=<p> index=<i>`.
 */
public class TreeVectorShape {

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

        @Override
        public String toString() {
            return "V(" + tag + ")";
        }
    }

    static int checks = 0;

    static void check(boolean cond, String what) {
        checks++;
        if (!cond) {
            throw new AssertionError(what);
        }
    }

    static long walkMap(String what, Map<K, V> view, long lo, long hi) {
        long prev = Long.MIN_VALUE, seen = 0, sum = 0;
        for (Iterator<Map.Entry<K, V>> it = view.entrySet().iterator(); it.hasNext();) {
            Map.Entry<K, V> e = it.next();
            Object ko = e.getKey();
            Object vo = e.getValue();
            check(ko instanceof K, what + ": key is a " + (ko == null ? "null" : ko.getClass().getName()));
            check(vo instanceof V, what + ": value is a " + (vo == null ? "null" : vo.getClass().getName()));
            K k = (K) ko;
            V v = (V) vo;
            check(k.payload == k.key * 3 + 1, what + ": corrupted key " + k);
            check(v.tag == k.key * 3 + 1, what + ": key " + k.key + " mapped to " + v);
            check(k.key >= prev, what + ": out of order " + k.key + " after " + prev);
            prev = k.key;
            sum += k.key;
            seen++;
        }
        check(seen == hi - lo, what + ": " + seen + " entries, expected " + (hi - lo));
        return sum;
    }

    static long walkSet(String what, SortedSet<K> view, long lo, long hi) {
        long prev = Long.MIN_VALUE, seen = 0, sum = 0;
        for (Iterator<K> it = view.iterator(); it.hasNext();) {
            Object ko = it.next();
            check(ko instanceof K, what + ": element is a " + (ko == null ? "null" : ko.getClass().getName()));
            K k = (K) ko;
            check(k.payload == k.key * 3 + 1, what + ": corrupted element " + k);
            check(k.key >= prev, what + ": out of order " + k.key + " after " + prev);
            prev = k.key;
            sum += k.key;
            seen++;
        }
        check(seen == hi - lo, what + ": " + seen + " elements, expected " + (hi - lo));
        return sum;
    }

    static TreeMap<K, V> m;
    static TreeSet<K> s;
    static long sum = 0;
    static final long LO = ENTRIES / 4;
    static final long HI = (3 * ENTRIES) / 4;

    static void phase(int p) {
        switch (p) {
            case 1:
                m = new TreeMap<K, V>();
                for (int i = 0; i < ENTRIES; i++) {
                    long k = (i * 7919) % ENTRIES;
                    m.put(new K(k), new V(k * 3 + 1));
                }
                break;
            case 2: check(m.size() == ENTRIES, "map size " + m.size()); break;
            case 3: sum += walkMap("headMap", m.headMap(new K(HI)), 0, HI); break;
            case 4: sum += walkMap("tailMap", m.tailMap(new K(LO)), LO, ENTRIES); break;
            case 5: sum += walkMap("subMap", m.subMap(new K(LO), new K(HI)), LO, HI); break;
            case 6: { NavigableMap<K, V> v = m.headMap(new K(HI), false);
                      sum += walkMap("headMap(k,false)", v, 0, HI); break; }
            case 7: { NavigableMap<K, V> v = m.tailMap(new K(LO), true);
                      sum += walkMap("tailMap(k,true)", v, LO, ENTRIES); break; }
            case 8: { NavigableMap<K, V> v = m.subMap(new K(LO), true, new K(HI), false);
                      sum += walkMap("subMap(k,t,k,f)", v, LO, HI); break; }
            case 9: { int n = 0;
                      for (K k : m.keySet()) {
                          check(k.payload == k.key * 3 + 1, "keySet: corrupted " + k);
                          n++;
                      }
                      check(n == ENTRIES, "keySet yielded " + n + " of " + ENTRIES); break; }
            case 10: check(m.toString().length() > ENTRIES, "toString truncated"); break;
            case 11: s = new TreeSet<K>();
                     for (int i = 0; i < ENTRIES; i++) { s.add(new K((i * 7919) % ENTRIES)); }
                     break;
            case 12: check(s.size() == ENTRIES, "set size " + s.size()); break;
            case 13: sum += walkSet("headSet", s.headSet(new K(HI)), 0, HI); break;
            case 14: sum += walkSet("tailSet", s.tailSet(new K(LO)), LO, ENTRIES); break;
            case 15: sum += walkSet("subSet", s.subSet(new K(LO), new K(HI)), LO, HI); break;
            case 16: { NavigableSet<K> v = s.subSet(new K(LO), true, new K(HI), false);
                       sum += walkSet("subSet(k,t,k,f)", v, LO, HI); break; }
            default: throw new IllegalArgumentException("no phase " + p);
        }
    }

    public static void main(String[] a) {
        String list = a.length > 0 ? a[0] : "1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16";
        String[] parts = list.split(",");
        for (int i = 0; i < parts.length; i++) {
            int p = Integer.parseInt(parts[i].trim());
            try {
                phase(p);
            } catch (Throwable t) {
                System.out.println("FAILAT phase=" + p + " index=" + (i + 1)
                        + " ex=" + t.getClass().getName()
                        + " msg=" + String.valueOf(t.getMessage()));
                throw t;
            }
        }
        System.out.println("OK " + list + " checks=" + checks + " sum=" + sum);
    }
}
