/**
 * Regression: every map native that walks a bucket chain must keep the chain
 * rooted across the Java callbacks it dispatches (HIB-MAPRESIZE-STALE.1).
 *
 * `RMapResizeGc` covers the RESIZE walk, whose only GC points are the
 * write-barrier stores it issues itself. This test covers the other half of the
 * bug class: `put`, `remove`, `get` and `containsKey` all walk a chain while
 * calling arbitrary user `hashCode()`/`equals()` between two dereferences of a
 * native-side Rust local. A key whose `equals` ALLOCATES turns each of those
 * callbacks into a guaranteed collection under `CRATONVM_DBG_GC_STRESS`, which
 * is what `RMapResizeGc`'s allocation-free key deliberately cannot do.
 *
 * The shapes that matter, all built below on purpose:
 *   - long chains (colliding hashes) so a walk makes MANY callbacks;
 *   - a removal at the HEAD, in the MIDDLE and at the TAIL of a chain, since
 *     the native takes different branches (bucket-slot store vs prev.next
 *     store) and each unlinks a node whose successors are momentarily reachable
 *     only from native locals;
 *   - a put that UPDATES an existing key (walk finds a match) and a put that
 *     APPENDS (walk runs off the end, then allocates a node), because only the
 *     second one allocates after the walk;
 *   - re-verification after every phase, so a chain that lost, duplicated or
 *     mis-linked an entry fails the checksum diff even when nothing crashes.
 *
 * Deterministic output; the runner diffs it against HotSpot.
 *
 * Run under stress with:
 *   CRATONVM_DBG_GC_STRESS=65536 cratonvm --nojit --Xmx 256m RMapGcStress
 */
import java.util.ArrayList;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Hashtable;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

public class RMapGcStress {
    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) throw new AssertionError(m);
    }

    /**
     * Key with a caller-chosen hash so bucket collisions are by design, and
     * with an ALLOCATING equals/hashCode so every native chain walk that
     * dispatches them is guaranteed to span a collection under GC stress.
     *
     * The allocation is a small array plus a String; both are young objects
     * that die immediately, which is exactly the traffic a moving young
     * collector relocates survivors around.
     */
    static final class K {
        final int id;
        final int h;

        K(int id, int h) {
            this.id = id;
            this.h = h;
        }

        @Override
        public int hashCode() {
            // Allocate, then return the caller-chosen hash unchanged so bucket
            // placement stays fully deterministic.
            int[] scratch = new int[8];
            for (int i = 0; i < scratch.length; i++) scratch[i] = h + i;
            return h + (scratch[0] - h);
        }

        @Override
        public boolean equals(Object o) {
            if (!(o instanceof K)) return false;
            // Allocate inside equals: this is the GC point the native's chain
            // cursors have to survive.
            StringBuilder sb = new StringBuilder(16);
            sb.append('K').append(id);
            String mine = sb.toString();
            String theirs = "K" + ((K) o).id;
            return mine.equals(theirs);
        }

        @Override
        public String toString() {
            return "K" + id;
        }
    }

    /**
     * Low 5 bits repeat every 32 keys, so every 32nd key collides while the
     * table is small; bit 8 alternates in blocks of 32, so those chains SPLIT
     * across the low/high halves once the table outgrows 256 rather than
     * migrating wholesale.
     */
    static int hashFor(int i) {
        return (i % 32) | ((i & 32) << 3);
    }

    static K key(int i) {
        return new K(i, hashFor(i));
    }

    static long fill(Map<K, String> m, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            String prev = m.put(key(i), "v" + i);
            check(prev == null, "fill: duplicate insert at " + i);
            sum += i;
        }
        return sum;
    }

    /** Full verification: targeted get, containsKey, size and a whole traversal. */
    static long verify(Map<K, String> m, Set<Integer> live, String what) {
        long acc = 0;
        for (int i : live) {
            String v = m.get(key(i));
            check(("v" + i).equals(v), what + ": lost/wrong value for " + i + " -> " + v);
            check(m.containsKey(key(i)), what + ": containsKey false for " + i);
            acc += v.length() + i;
        }
        check(m.size() == live.size(), what + ": size " + m.size() + " != " + live.size());
        int iterated = 0;
        long keySum = 0;
        for (Map.Entry<K, String> e : m.entrySet()) {
            K k = e.getKey();
            String v = e.getValue();
            check(k != null, what + ": null key during iteration");
            check(v != null, what + ": null value during iteration");
            check(live.contains(k.id), what + ": iterated a removed key " + k.id);
            check(("v" + k.id).equals(v), what + ": iteration value mismatch for " + k.id);
            keySum += k.id;
            iterated++;
        }
        check(iterated == live.size(), what + ": iterated " + iterated + " != " + live.size());
        return acc + keySum;
    }

    /**
     * `put` on keys already present: the native walks the chain, dispatches
     * `equals` at every hash-matching node, and then writes into the node it
     * matched. A cursor that went stale during one of those callbacks updates
     * the wrong object (or a freed one).
     */
    static long updateInPlace(Map<K, String> m, Set<Integer> live) {
        long acc = 0;
        List<Integer> ids = new ArrayList<>(live);
        for (int i : ids) {
            String prev = m.put(key(i), "v" + i);
            check(("v" + i).equals(prev), "update: put returned " + prev + " for " + i);
            acc += i;
        }
        return acc;
    }

    /**
     * Remove a deterministic mix that hits the head, the middle and the tail of
     * the chains built above. `step` is coprime with the 32-key collision
     * period for the first pass and a divisor of it for the second, so both a
     * scattered and a whole-chain-collapsing removal pattern run.
     */
    static long removeSome(Map<K, String> m, Set<Integer> live, int n, int start, int step,
                           String what) {
        long removed = 0;
        for (int i = start; i < n; i += step) {
            if (!live.contains(i)) continue;
            String v = m.remove(key(i));
            check(("v" + i).equals(v), what + ": remove returned " + v + " for " + i);
            check(!m.containsKey(key(i)), what + ": key " + i + " still present after remove");
            live.remove(i);
            removed++;
        }
        return removed;
    }

    static long exercise(Map<K, String> m, int n, String what) {
        Set<Integer> live = new HashSet<>();
        long sum = fill(m, n);
        for (int i = 0; i < n; i++) live.add(i);
        long acc = verify(m, live, what + "/filled");

        acc += updateInPlace(m, live);
        acc += verify(m, live, what + "/updated");

        // Head-ish removals first (every 32nd key shares a bucket, so removing
        // i%32==0 empties whole chain heads), then a scattered pass.
        long r1 = removeSome(m, live, n, 0, 32, what + "/head");
        acc += verify(m, live, what + "/after-head-removals");
        long r2 = removeSome(m, live, n, 7, 3, what + "/scattered");
        acc += verify(m, live, what + "/after-scattered-removals");

        // Re-insert everything that was removed: the chains now re-grow through
        // the tail-append path over buckets that were emptied and re-linked.
        for (int i = 0; i < n; i++) {
            if (!live.contains(i)) {
                check(m.put(key(i), "v" + i) == null, what + ": reinsert saw a stale entry at " + i);
                live.add(i);
            }
        }
        acc += verify(m, live, what + "/reinserted");
        return acc + r1 * 31 + r2 * 17;
    }

    public static void main(String[] args) {
        final int n = args.length > 0 ? Integer.parseInt(args[0]) : 3000;

        long hm = exercise(new HashMap<K, String>(), n, "HashMap");
        long lhm = exercise(new LinkedHashMap<K, String>(), n, "LinkedHashMap");
        long chm = exercise(new ConcurrentHashMap<K, String>(), n, "ConcurrentHashMap");
        long ht = exercise(new Hashtable<K, String>(), n, "Hashtable");

        // HashSet rides the same node chains through a different native entry.
        Set<K> set = new HashSet<>();
        for (int i = 0; i < n; i++) check(set.add(key(i)), "set: duplicate add at " + i);
        for (int i = 0; i < n; i++) check(!set.add(key(i)), "set: re-add reported new at " + i);
        long setAcc = 0;
        for (int i = 0; i < n; i += 5) {
            check(set.remove(key(i)), "set: remove missed " + i);
            setAcc += i;
        }
        check(set.size() == n - ((n + 4) / 5), "set: size after removals");
        for (K k : set) {
            check(k.id % 5 != 0, "set: iterated a removed key " + k.id);
            setAcc += k.id;
        }

        System.out.println("CK n=" + n + " hm=" + hm + " lhm=" + lhm + " chm=" + chm
                + " ht=" + ht + " set=" + setAcc);
        System.out.println("PASS RMapGcStress (" + checks + " checks)");
    }
}
