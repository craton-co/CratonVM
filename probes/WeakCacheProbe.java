import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.WeakHashMap;

/**
 * javac's type-system caches are WeakHashMap-keyed on Symbol/Type objects that
 * the compiler holds STRONGLY for the whole compilation. If a VM clears those
 * weak references while the key is still strongly reachable, every cache read
 * misses and javac recomputes membersClosure/descriptors from scratch — which
 * is exactly the shape of the AOT throughput wall's profile.
 *
 * Every row is a fact about retention, printed identically on any correct VM.
 */
public class WeakCacheProbe {

    static void p(String k, Object v) {
        System.out.println(k + " = " + v);
    }

    static final class Key {
        final int id;
        Key(int id) { this.id = id; }
        @Override public String toString() { return "Key#" + id; }
    }

    public static void main(String[] args) throws Exception {
        // ---- W: a WeakHashMap whose keys are all STRONGLY held ------------
        // Nothing here is collectable, so a correct VM keeps all 2000 entries
        // no matter how much garbage is produced alongside.
        List<Key> strong = new ArrayList<>();
        Map<Key, String> cache = new WeakHashMap<>();
        for (int i = 0; i < 2000; i++) {
            Key k = new Key(i);
            strong.add(k);
            cache.put(k, "v" + i);
        }
        p("W01 size before churn", cache.size());
        churn(40);
        p("W02 size after churn", cache.size());
        System.gc();
        churn(40);
        System.gc();
        p("W03 size after gc+churn", cache.size());
        int hits = 0;
        for (Key k : strong) {
            if (cache.get(k) != null) {
                hits++;
            }
        }
        p("W04 live-key hits (must be 2000)", hits);
        p("W05 strong list still 2000", strong.size());

        // ---- D: keys that ARE droppable must eventually go ----------------
        // The opposite error — never clearing — is also wrong, and a probe
        // that only checked retention would call a leak a pass.
        Map<Key, String> dropping = new WeakHashMap<>();
        for (int i = 0; i < 5000; i++) {
            dropping.put(new Key(i), "v" + i);
        }
        int before = dropping.size();
        for (int i = 0; i < 12 && dropping.size() == before; i++) {
            System.gc();
            churn(20);
        }
        p("D01 unreachable keys were cleared", dropping.size() < before);

        // ---- R: a plain WeakReference to a strongly-held referent ---------
        Object held = new Object();
        WeakReference<Object> wr = new WeakReference<>(held);
        churn(40);
        System.gc();
        churn(40);
        p("R01 weak ref to held object survives", wr.get() != null);

        Object soft = new Object();
        SoftReference<Object> sr = new SoftReference<>(soft);
        churn(40);
        System.gc();
        p("R02 soft ref to held object survives", sr.get() != null);

        // A SoftReference whose referent is dropped must NOT be cleared by a
        // single gc under no memory pressure — javac leans on this for its
        // bigger caches.
        SoftReference<Object> sr2 = new SoftReference<>(new Object());
        System.gc();
        p("R03 soft ref survives one gc with no pressure", sr2.get() != null);

        // ---- Q: the queue must not receive a live referent ----------------
        ReferenceQueue<Object> q = new ReferenceQueue<>();
        Object live = new Object();
        WeakReference<Object> qr = new WeakReference<>(live, q);
        churn(40);
        System.gc();
        churn(40);
        p("Q01 live referent not enqueued", q.poll() == null);
        p("Q02 live referent still readable", qr.get() != null);
        p("Q03 referent identity preserved", qr.get() == live);

        // ---- C: the shape javac actually runs -----------------------------
        // A memoizing closure over a stable key set. On a VM that retains
        // correctly this computes each value ONCE; the miss count IS the
        // regression signal.
        int[] computed = {0};
        Map<Key, Integer> memo = new WeakHashMap<>();
        List<Key> keys = new ArrayList<>();
        for (int i = 0; i < 500; i++) {
            keys.add(new Key(i));
        }
        for (int round = 0; round < 20; round++) {
            for (Key k : keys) {
                Integer v = memo.get(k);
                if (v == null) {
                    computed[0]++;
                    v = k.id * 2;
                    memo.put(k, v);
                }
            }
            churn(4);
        }
        p("C01 computations for 500 keys x 20 rounds (must be 500)", computed[0]);
        p("C02 memo retained all keys", memo.size());

        // ---- H: the same shape on a STRONG map, as the control ------------
        // If C01 is 500 here and 10000 above, the map is the variable; if both
        // are wrong, the fault is not in reference handling at all.
        int[] strongComputed = {0};
        Map<Key, Integer> strongMemo = new HashMap<>();
        for (int round = 0; round < 20; round++) {
            for (Key k : keys) {
                if (strongMemo.get(k) == null) {
                    strongComputed[0]++;
                    strongMemo.put(k, k.id * 2);
                }
            }
            churn(4);
        }
        p("H01 strong-map computations (must be 500)", strongComputed[0]);
    }

    /** Allocate enough short-lived garbage to provoke young collections. */
    static void churn(int mb) {
        Object sink = null;
        for (int i = 0; i < mb * 16; i++) {
            sink = new byte[64 * 1024];
            if (sink.hashCode() == 42) {
                System.out.print("");
            }
        }
        Reference.reachabilityFence(sink);
    }
}
