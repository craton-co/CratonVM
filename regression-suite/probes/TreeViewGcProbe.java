// Which TreeMap/TreeSet view survives a collection, and which does not?
//
// **THIS PROBE DOES NOT REPRODUCE `RTreeRangeGc`'s FAILURE. It is the NEGATIVE
// CONTROL SET**, and that is why it is committed: nine view shapes, each walked
// while the heap is under real collection pressure, all of which SURVIVE. The
// defect needs something none of them has — see `WORKER-5-NOTE-10` for the
// bisect that localises it and for everything ruled out.
//
// `RTreeRangeGc` fails under `--Xmx 64m` in COMPATIBLE mode (100% with
// `--nojit`, which is the reproduction to use) with
//
//     ClassCastException: RTreeRangeGc$V cannot be cast to java.util.Map$Entry
//
// i.e. the entrySet iterator handed back a VALUE object where an Entry was
// expected — an address that was reclaimed and reused. It passes under
// `--jdk-only`, where the collection natives decline and real JDK bytecode
// runs, so the native view-materialisation path is implicated.
//
// Each case walks ONE view and allocates hard between `next()` calls, both via
// `churn()` and via `eagerMessage()`, which reproduces the vector's habit of
// building an assertion message (with `getClass().getName()`) on every element
// while the entry, key and value are live in interpreter locals.
//
//   java  -cp <dir> TreeViewGcProbe <case>          # the oracle
//   cratonvm --Xmx 64m --nojit -cp <dir> TreeViewGcProbe <case>
//
// case: entryset | keyset | values | headmap | tailmap | submap
//       | headset | tailset | subset | all
public class TreeViewGcProbe {

    static final int N = 400;
    /** Churn per element — enough to force a collection inside a 64m heap. */
    static final int CHURN = 200;

    static final class K implements Comparable<K> {
        final int key;
        final int payload;
        K(int key) { this.key = key; this.payload = key * 3 + 1; }
        public int compareTo(K o) { return Integer.compare(key, o.key); }
        public boolean equals(Object o) { return o instanceof K && ((K) o).key == key; }
        public int hashCode() { return key; }
        public String toString() { return "K(" + key + ")"; }
    }

    static final class V {
        final int tag;
        V(int tag) { this.tag = tag; }
        public String toString() { return "V(" + tag + ")"; }
    }

    static Object sink;

    /** Allocate enough that a small heap must collect. */
    static void churn() {
        for (int i = 0; i < CHURN; i++) {
            sink = new V(i);
            sink = new K(i);
        }
    }

    /**
     * The allocation shape `RTreeRangeGc` actually has, and the reason the
     * first version of this probe could not reproduce it.
     *
     * Its `check(cond, msg)` takes the message as an ARGUMENT, so
     * `what + ": key is a " + ko.getClass().getName()` is built EAGERLY on
     * every element even when the check passes — six times per entry. That
     * allocates a class-name String and a StringBuilder chain **while the
     * entry and its key and value are live in interpreter locals**, which is
     * the state the churn-between-elements version never reached.
     */
    static void eagerMessage(Object entry, Object k, Object v) {
        sink = "e=" + (entry == null ? "null" : entry.getClass().getName())
                + " k=" + (k == null ? "null" : k.getClass().getName())
                + " v=" + (v == null ? "null" : v.getClass().getName())
                + " kk=" + k + " vv=" + v;
    }

    static java.util.TreeMap<K, V> map() {
        java.util.TreeMap<K, V> m = new java.util.TreeMap<>();
        for (int i = 0; i < N; i++) m.put(new K(i), new V(i * 3 + 1));
        return m;
    }

    static java.util.TreeSet<K> set() {
        java.util.TreeSet<K> s = new java.util.TreeSet<>();
        for (int i = 0; i < N; i++) s.add(new K(i));
        return s;
    }

    /** Walk an entry view, churning between elements. Reports the first break. */
    static String walkEntries(java.util.Set<java.util.Map.Entry<K, V>> es, int expect) {
        int seen = 0;
        for (java.util.Iterator<java.util.Map.Entry<K, V>> it = es.iterator(); it.hasNext();) {
            Object raw = it.next();
            if (!(raw instanceof java.util.Map.Entry)) {
                return "BROKE at " + seen + ": next() gave a "
                        + (raw == null ? "null" : raw.getClass().getName());
            }
            java.util.Map.Entry<?, ?> e = (java.util.Map.Entry<?, ?>) raw;
            Object k = e.getKey(), v = e.getValue();
            if (!(k instanceof K)) {
                return "BROKE at " + seen + ": key is a "
                        + (k == null ? "null" : k.getClass().getName());
            }
            if (!(v instanceof V)) {
                return "BROKE at " + seen + ": value is a "
                        + (v == null ? "null" : v.getClass().getName());
            }
            if (((V) v).tag != ((K) k).key * 3 + 1) {
                return "BROKE at " + seen + ": " + k + " -> " + v + " (mismatched)";
            }
            eagerMessage(raw, k, v);
            seen++;
            churn();
        }
        return seen == expect ? "ok (" + seen + ")" : "SHORT: " + seen + " of " + expect;
    }

    /** Walk a key view, churning between elements. */
    static String walkKeys(java.util.Collection<K> ks, int expect) {
        int seen = 0;
        for (java.util.Iterator<K> it = ks.iterator(); it.hasNext();) {
            Object raw = it.next();
            if (!(raw instanceof K)) {
                return "BROKE at " + seen + ": next() gave a "
                        + (raw == null ? "null" : raw.getClass().getName());
            }
            K k = (K) raw;
            if (k.payload != k.key * 3 + 1) {
                return "BROKE at " + seen + ": corrupted " + k + " payload=" + k.payload;
            }
            eagerMessage(raw, k, k);
            seen++;
            churn();
        }
        return seen == expect ? "ok (" + seen + ")" : "SHORT: " + seen + " of " + expect;
    }

    static String walkValues(java.util.Collection<V> vs, int expect) {
        int seen = 0;
        for (java.util.Iterator<V> it = vs.iterator(); it.hasNext();) {
            Object raw = it.next();
            if (!(raw instanceof V)) {
                return "BROKE at " + seen + ": next() gave a "
                        + (raw == null ? "null" : raw.getClass().getName());
            }
            seen++;
            churn();
        }
        return seen == expect ? "ok (" + seen + ")" : "SHORT: " + seen + " of " + expect;
    }

    static void one(String name, String result) {
        System.out.println(String.format("  %-24s %s", name, result));
    }

    public static void main(String[] args) {
        String which = args.length > 0 ? args[0] : "all";
        boolean all = which.equals("all");
        int hi = N / 2;
        System.out.println("TreeViewGcProbe N=" + N + " churn=" + CHURN + " case=" + which);

        // The WHOLE-MAP views first: if these are broken, the range views are
        // not the story and the bug is in the base materialisation.
        if (all || which.equals("entryset")) {
            one("map.entrySet", walkEntries(map().entrySet(), N));
        }
        if (all || which.equals("keyset")) {
            one("map.keySet", walkKeys(map().keySet(), N));
        }
        if (all || which.equals("values")) {
            one("map.values", walkValues(map().values(), N));
        }
        // The RANGE views — what RTreeRangeGc walks first.
        if (all || which.equals("headmap")) {
            one("headMap.entrySet", walkEntries(map().headMap(new K(hi)).entrySet(), hi));
        }
        if (all || which.equals("tailmap")) {
            one("tailMap.entrySet", walkEntries(map().tailMap(new K(hi)).entrySet(), N - hi));
        }
        if (all || which.equals("submap")) {
            one("subMap.entrySet",
                    walkEntries(map().subMap(new K(10), new K(hi)).entrySet(), hi - 10));
        }
        if (all || which.equals("headset")) {
            one("headSet", walkKeys(set().headSet(new K(hi)), hi));
        }
        if (all || which.equals("tailset")) {
            one("tailSet", walkKeys(set().tailSet(new K(hi)), N - hi));
        }
        if (all || which.equals("subset")) {
            one("subSet", walkKeys(set().subSet(new K(10), new K(hi)), hi - 10));
        }
        System.out.println("TreeViewGcProbe done");
    }
}
