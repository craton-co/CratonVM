// gcg g2/k: multi-threaded TreeMap / LinkedHashMap / LinkedList churn, the
// overlay operations whose owner registration
// `CRATONVM_OVERLAY_OWNER_ONCE_PER_SLOT` takes off the steady state.
//
// Each thread keeps a few long-lived collections it mutates every round (the
// steady-state slot hits) and creates and drops short-lived ones (the mints
// and the prune), with enough allocation that collections move and prune the
// overlay rows between operations. Every collection's contents are checked
// at the end: an owner row lost by the opt-in would let a collection's
// backing be freed under it, which reads back wrong or crashes.
//
//   GcgK2OverlayChurnProbe <threads> <rounds>
//
// stdout (deterministic): one K2OVERLAY_OK line with a checksum.
// stderr: wall time.
import java.util.LinkedHashMap;
import java.util.LinkedList;
import java.util.Map;
import java.util.TreeMap;
import java.util.concurrent.atomic.AtomicLong;

public final class GcgK2OverlayChurnProbe {
    static volatile Object sink;

    public static void main(String[] a) throws Exception {
        final int threads = Integer.parseInt(a[0]);
        final int rounds = Integer.parseInt(a[1]);
        final AtomicLong sum = new AtomicLong();
        final AtomicLong bad = new AtomicLong();
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                TreeMap<Integer, String> tree = new TreeMap<>();
                LinkedHashMap<Integer, Integer> lhm = new LinkedHashMap<>();
                LinkedList<Integer> list = new LinkedList<>();
                for (int r = 0; r < rounds; r++) {
                    int k = (r * 7 + id) % 512;
                    tree.put(k, "v" + r);
                    lhm.put(k, r);
                    list.addLast(r);
                    if (list.size() > 256) {
                        list.removeFirst();
                    }
                    TreeMap<Integer, Integer> brief = new TreeMap<>();
                    for (int j = 0; j < 8; j++) {
                        brief.put(j, r + j);
                    }
                    if (brief.size() != 8 || brief.firstKey() != 0) {
                        bad.incrementAndGet();
                    }
                    sink = new byte[1024];
                }
                long local = 0;
                for (Map.Entry<Integer, String> e : tree.entrySet()) {
                    local += e.getKey() + e.getValue().length();
                }
                for (Map.Entry<Integer, Integer> e : lhm.entrySet()) {
                    local += e.getKey() * 3L + e.getValue();
                }
                for (int v : list) {
                    local += v;
                }
                sum.addAndGet(local);
            }, "k2-ov-" + t);
        }
        long t0 = System.nanoTime();
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("K2OVERLAY_OK threads=" + threads + " rounds=" + rounds
                + " sum=" + sum.get() + " bad=" + bad.get());
        System.err.println("[k2-overlay] ms=" + ms);
    }
}
