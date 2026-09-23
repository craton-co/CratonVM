// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Iterator;
import java.util.Map;
import java.util.NavigableMap;
import java.util.NavigableSet;
import java.util.SortedSet;
import java.util.TreeMap;
import java.util.TreeSet;

/**
 * TreeMap/TreeSet range views over a custom Comparable, built under GC pressure.
 *
 * CratonVM's headMap/tailMap/subMap natives (and the TreeSet twins) snapshot the
 * source collection into a bare Rust Vec of raw ObjectRefs and then walk it,
 * calling tree_compare - which for anything that is not a String or a
 * homogeneous primitive wrapper dispatches the key's REAL, interpreted
 * compareTo - and native_tm_put. Both allocate. Entries the walk has not reached
 * yet were never refreshed, so a young collection mid-walk left the rest of the
 * loop dereferencing relocated addresses. The observed failure is
 *
 *   ClassCastException: class java.lang.Object cannot be cast to
 *   class java.lang.Comparable
 *
 * raised out of headMap, because the stale key decoded as whatever object now
 * occupied its old address. The same snapshot-then-allocate shape was fixed at
 * the same time in keySet/entrySet/toString/iterator and in the TreeSet range
 * views, which reused an element read before the comparison that moved it.
 *
 * compareTo() allocates deliberately so the collection lands inside the native.
 *
 * REQUIRED CratonVM ARGUMENT: --Xmx 64m, and only that (see run.sh
 * class_cv_args). On the default heap no collection happens during the walk at
 * all and the class passes on a broken VM. Unlike RPriorityQueueGc this one
 * does NOT need --nojit - it reproduces with the JIT on, 3/3 (b2e13e441), so
 * registering it leaves the default compiling configuration under test.
 * HotSpot deliberately does not get the flag: it is a CratonVM spelling and
 * the expected output does not depend on the heap size.
 */
public class RTreeRangeGc {

    static final int ENTRIES = 400;

    static volatile Object sink;

    static void churn() {
        Object[] junk = new Object[32];
        for (int i = 0; i < junk.length; i++) {
            junk[i] = new byte[256];
        }
        sink = junk;
    }

    /** Two long fields: neither a String nor a single-field primitive wrapper. */
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

    /**
     * Count of assertions executed, published on a CK line so the cross-VM diff
     * can see a run that silently asserted fewer things than the oracle (harness
     * guard G3; see regression-suite/harness-guard.sh and
     * docs/known-issues/jdk-only/W7-60-harness-extract-blindness.md).
     *
     * The UNIT is one check() call, the same unit every other vector in the
     * suite publishes — deliberately not "one view" or "one phase". checkMap and
     * checkSet assert per ELEMENT inside a walk, which is what makes the number
     * load-bearing here rather than decorative: a range view that hands back
     * FEWER entries than it should — the empty-view failure this vector exists
     * to catch — runs fewer per-element checks and reports a smaller number, so
     * it diverges from the oracle's count as well as tripping the local
     * `seen == hi - lo` assertion. Every loop below is over a fixed-size
     * collection, so on a healthy VM the number is a constant.
     */
    static int checks = 0;

    static void check(boolean cond, String what) {
        checks++;
        if (!cond) {
            throw new AssertionError("RTreeRangeGc: " + what);
        }
    }

    /** Every entry intact, ascending, keys within [lo,hi). Returns the key sum. */
    static long checkMap(String what, Map<K, V> view, long lo, long hi) {
        long prev = Long.MIN_VALUE;
        long seen = 0;
        long sum = 0;
        for (Iterator<Map.Entry<K, V>> it = view.entrySet().iterator(); it.hasNext();) {
            Map.Entry<K, V> e = it.next();
            Object ko = e.getKey();
            Object vo = e.getValue();
            check(ko instanceof K,
                    what + ": key is a " + (ko == null ? "null" : ko.getClass().getName()));
            check(vo instanceof V,
                    what + ": value is a " + (vo == null ? "null" : vo.getClass().getName()));
            K k = (K) ko;
            V v = (V) vo;
            check(k.payload == k.key * 3 + 1, what + ": corrupted key " + k);
            check(v.tag == k.key * 3 + 1, what + ": key " + k.key + " mapped to " + v);
            check(k.key >= prev, what + ": out of order, " + k.key + " after " + prev);
            check(k.key >= lo && k.key < hi,
                    what + ": key " + k.key + " outside [" + lo + "," + hi + ")");
            prev = k.key;
            sum += k.key;
            seen++;
        }
        check(seen == hi - lo, what + ": " + seen + " entries, expected " + (hi - lo));
        return sum;
    }

    static long checkSet(String what, SortedSet<K> view, long lo, long hi) {
        long prev = Long.MIN_VALUE;
        long seen = 0;
        long sum = 0;
        for (Iterator<K> it = view.iterator(); it.hasNext();) {
            Object ko = it.next();
            check(ko instanceof K,
                    what + ": element is a " + (ko == null ? "null" : ko.getClass().getName()));
            K k = (K) ko;
            check(k.payload == k.key * 3 + 1, what + ": corrupted element " + k);
            check(k.key >= prev, what + ": out of order, " + k.key + " after " + prev);
            check(k.key >= lo && k.key < hi,
                    what + ": element " + k.key + " outside [" + lo + "," + hi + ")");
            prev = k.key;
            sum += k.key;
            seen++;
        }
        check(seen == hi - lo, what + ": " + seen + " elements, expected " + (hi - lo));
        return sum;
    }

    public static void main(String[] args) {
        long lo = ENTRIES / 4;
        long hi = (3 * ENTRIES) / 4;

        TreeMap<K, V> m = new TreeMap<K, V>();
        for (int i = 0; i < ENTRIES; i++) {
            long k = (i * 7919) % ENTRIES;
            m.put(new K(k), new V(k * 3 + 1));
        }
        check(m.size() == ENTRIES, "map size " + m.size());

        long sum = 0;
        sum += checkMap("headMap", m.headMap(new K(hi)), 0, hi);
        sum += checkMap("tailMap", m.tailMap(new K(lo)), lo, ENTRIES);
        sum += checkMap("subMap", m.subMap(new K(lo), new K(hi)), lo, hi);
        NavigableMap<K, V> h2 = m.headMap(new K(hi), false);
        sum += checkMap("headMap(k,false)", h2, 0, hi);
        NavigableMap<K, V> t2 = m.tailMap(new K(lo), true);
        sum += checkMap("tailMap(k,true)", t2, lo, ENTRIES);
        NavigableMap<K, V> s2 = m.subMap(new K(lo), true, new K(hi), false);
        sum += checkMap("subMap(k,true,k,false)", s2, lo, hi);
        System.out.println("CK tm-range " + sum);

        // keySet / toString walk the same snapshot through allocating code.
        long keySum = 0;
        int keyCount = 0;
        for (K k : m.keySet()) {
            check(k.payload == k.key * 3 + 1, "keySet: corrupted " + k);
            keySum += k.key;
            keyCount++;
        }
        // An empty keySet() would walk no elements, assert nothing, and print a
        // CK line that only the HotSpot diff could catch - and run.sh skips that
        // diff when no HotSpot is present.
        check(keyCount == ENTRIES, "keySet yielded " + keyCount + " of " + ENTRIES);
        System.out.println("CK tm-keyset " + keySum);
        check(m.toString().length() > ENTRIES, "toString truncated");

        TreeSet<K> s = new TreeSet<K>();
        for (int i = 0; i < ENTRIES; i++) {
            s.add(new K((i * 7919) % ENTRIES));
        }
        check(s.size() == ENTRIES, "set size " + s.size());
        long setSum = 0;
        setSum += checkSet("headSet", s.headSet(new K(hi)), 0, hi);
        setSum += checkSet("tailSet", s.tailSet(new K(lo)), lo, ENTRIES);
        setSum += checkSet("subSet", s.subSet(new K(lo), new K(hi)), lo, hi);
        NavigableSet<K> ns = s.subSet(new K(lo), true, new K(hi), false);
        setSum += checkSet("subSet(k,true,k,false)", ns, lo, hi);
        System.out.println("CK ts-range " + setSum);

        System.out.println("CK RTreeRangeGc checks=" + checks);
        System.out.println("PASS RTreeRangeGc (" + checks + " checks)");
    }
}
