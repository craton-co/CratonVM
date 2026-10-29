/**
 * Regression: HashMap/ConcurrentHashMap resize under GC pressure
 * (HIB-MAPRESIZE-STALE.1).
 *
 * CratonVM reimplements `HashMap.put` natively, and its resize walks each old
 * bucket's chain splitting it into a "low" and a "high" list. The links it
 * writes are REFERENCE-typed field/array stores, so each can allocate a
 * remembered-set entry through the write barrier and therefore complete a
 * moving young GC mid-walk. The walk's cursors (`lo_head`/`lo_tail`/
 * `hi_head`/`hi_tail`, the bucket arrays, the current node) are native-side
 * Rust locals invisible to root scanning, so a collection there used to leave
 * the resized table holding pre-move addresses — dangling chain heads that
 * every later lookup walked into, ending in SIGSEGV.
 *
 * The shape that matters is a chain of SEVERAL nodes in one bucket that then
 * SPLITS across the resize, because the corrupting store only happens once a
 * partition already has a head. Colliding hash codes below build exactly that,
 * and the allocation per entry keeps the collector busy.
 *
 * Run under `CRATONVM_DBG_GC_STRESS=<small>` to force a young GC every few
 * bytes of allocation; that turns the race into a deterministic failure.
 *
 * Prints deterministic checksums; the runner diffs them against HotSpot, so a
 * lost, duplicated or mis-linked entry fails the diff even if it does not
 * crash.
 */
import java.util.HashMap;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;

public class RMapResizeGc {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    /** Key whose hash we control exactly, so bucket collisions are by design. */
    static final class K {
        final int id;
        final int h;
        K(int id, int h) { this.id = id; this.h = h; }
        @Override public int hashCode() { return h; }
        @Override public boolean equals(Object o) { return (o instanceof K) && ((K) o).id == id; }
        @Override public String toString() { return "K" + id; }
    }

    /**
     * Low 6 bits repeat every 64 keys, so while the table is small every 64th
     * key collides into one bucket and builds a long chain. Bit 9 alternates
     * every 64 keys, so when the table grows past 512 those chains split
     * between the low and high halves instead of moving wholesale — the case
     * that exercises the lo/hi tail links.
     */
    static int hashFor(int i) { return (i % 64) | ((i & 64) << 3); }

    static long fill(Map<K, String> m, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            m.put(new K(i, hashFor(i)), "v" + i);
            sum += i;
        }
        return sum;
    }

    static long verify(Map<K, String> m, int n, String what) {
        long found = 0;
        for (int i = 0; i < n; i++) {
            String v = m.get(new K(i, hashFor(i)));
            check(("v" + i).equals(v), what + ": lost or wrong value for key " + i + " -> " + v);
            found += v.length();
        }
        check(m.size() == n, what + ": size " + m.size() + " != " + n);
        // Full traversal: a dangling or truncated chain shows up here even when
        // a targeted get() happens to take a different path.
        long iterated = 0;
        for (Map.Entry<K, String> e : m.entrySet()) {
            check(e.getKey() != null, what + ": null key during iteration");
            check(e.getValue() != null, what + ": null value during iteration");
            iterated++;
        }
        check(iterated == n, what + ": iterated " + iterated + " != " + n);
        return found;
    }

    public static void main(String[] args) {
        final int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;

        HashMap<K, String> hm = new HashMap<>();
        long hmSum = fill(hm, n);
        long hmLen = verify(hm, n, "HashMap");

        ConcurrentHashMap<K, String> chm = new ConcurrentHashMap<>();
        long chmSum = fill(chm, n);
        long chmLen = verify(chm, n, "ConcurrentHashMap");

        // Removal churn re-links chains; do it after the growth so the table is
        // large and the chains that remain were built across several resizes.
        long removed = 0;
        for (int i = 0; i < n; i += 3) {
            if (hm.remove(new K(i, hashFor(i))) != null) removed++;
        }
        check(hm.size() == n - removed, "HashMap: size after removals");
        long survivors = 0;
        for (Map.Entry<K, String> e : hm.entrySet()) survivors++;
        check(survivors == n - removed, "HashMap: iteration after removals");

        System.out.println("CK n=" + n + " hmSum=" + hmSum + " hmLen=" + hmLen
                + " chmSum=" + chmSum + " chmLen=" + chmLen
                + " removed=" + removed + " survivors=" + survivors);
        System.out.println("PASS RMapResizeGc (" + checks + " checks)");
    }
}
