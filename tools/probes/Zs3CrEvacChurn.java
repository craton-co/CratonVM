import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * zs s3/cr: soundness and cost rows for the evacuating slide
 * ({@code CRATONVM_ZGC_SLIDE_EVACUATE=1}), its quarantine
 * ({@code CRATONVM_ZGC_EVAC_QUARANTINE=1}) and the {@code =2} gate priced on
 * the selection ({@code CRATONVM_ZGC_SELECTIVE_GATE_ON_SELECTION=1}).
 *
 * <p>Each round builds a MOSAIC (every {@code keep}-th object of a long mixed
 * run is retained, so the retained pages are fragmented and the selector picks
 * them), links the retained objects to each other and to OLDER rounds' objects
 * through fields and reference arrays (edges that point into pages the same
 * collection evacuates), mutates a slice of those edges after a collection
 * (stores into objects that just moved), and checks after every round that:
 * <ul>
 *   <li>every node's payload, tag and array contents are what was written;</li>
 *   <li>every node's identity hash is the one recorded before it moved;</li>
 *   <li>a weak reference to a strongly reachable node still answers it.</li>
 * </ul>
 * stdout is deterministic (checksums and counts, no hashes) and must equal
 * HotSpot's byte for byte; CPU time (all threads) goes to stderr.
 *
 * <p>Usage: {@code Zs3CrEvacChurn [rounds] [perRound] [keep]}
 * (defaults 30 40000 8).
 */
public final class Zs3CrEvacChurn {

    static final class Node {
        Node next;
        Node older;
        Object[] refs;
        final long[] payload;
        final int tag;
        int hash;

        Node(int tag, int payloadLongs) {
            this.tag = tag;
            this.payload = new long[payloadLongs];
            for (int i = 0; i < payloadLongs; i++) {
                payload[i] = (long) tag * 1_000_003L + i;
            }
        }
    }

    static long check(List<Node> all, int[] bad) {
        long sum = 0;
        for (int i = 0; i < all.size(); i++) {
            Node n = all.get(i);
            if (n.tag != i) {
                bad[0]++;
            }
            for (int k = 0; k < n.payload.length; k++) {
                if (n.payload[k] != (long) n.tag * 1_000_003L + k) {
                    bad[0]++;
                    break;
                }
            }
            if (System.identityHashCode(n) != n.hash) {
                bad[1]++;
            }
            if (n.refs != null) {
                for (Object o : n.refs) {
                    if (o instanceof Node m) {
                        sum += m.tag;
                    } else if (o instanceof String s) {
                        sum += s.length();
                    }
                }
            }
            sum = sum * 31 + n.tag + (n.next != null ? n.next.tag : -1)
                    + (n.older != null ? n.older.tag : -2);
        }
        return sum;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 30;
        int perRound = args.length > 1 ? Integer.parseInt(args[1]) : 40_000;
        int keep = args.length > 2 ? Integer.parseInt(args[2]) : 8;
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long cpu0 = mx.getCurrentThreadCpuTime();

        List<Node> all = new ArrayList<>();
        List<WeakReference<Node>> weak = new ArrayList<>();
        long rng = 0x9E3779B97F4A7C15L;
        Object sink = null;
        int[] bad = new int[2];
        int weakLost = 0;
        long total = 0;
        for (int r = 0; r < rounds; r++) {
            Node prev = null;
            for (int i = 0; i < perRound; i++) {
                rng = rng * 6364136223846793005L + 1442695040888963407L;
                int shape = (int) ((rng >>> 33) & 7);
                if (i % keep != 0) {
                    // Garbage of mixed sizes between the survivors.
                    sink = (shape < 5) ? new long[1 + shape * 3] : new Object[2 + shape];
                    continue;
                }
                Node n = new Node(all.size(), 1 + (shape & 3));
                n.hash = System.identityHashCode(n);
                n.next = prev;
                if (!all.isEmpty()) {
                    n.older = all.get((int) ((rng >>> 20) % all.size()));
                }
                if (shape >= 4) {
                    n.refs = new Object[shape];
                    for (int k = 0; k < shape; k++) {
                        n.refs[k] = (k % 3 == 0) ? ("s" + (n.tag % 97)) : (k % 3 == 1 ? n.older : prev);
                    }
                }
                all.add(n);
                if ((n.tag & 1023) == 0) {
                    weak.add(new WeakReference<>(n));
                }
                prev = n;
            }
            System.gc();
            // Stores into objects the collection may have just evacuated.
            for (int i = r; i < all.size(); i += 53) {
                Node n = all.get(i);
                n.older = all.get((i * 7 + r) % all.size());
                if (n.refs != null) {
                    n.refs[0] = n.older;
                }
            }
            total = check(all, bad);
            for (WeakReference<Node> w : weak) {
                if (w.get() == null) {
                    weakLost++;
                }
            }
        }
        System.out.println("ZS3CR evac rounds=" + rounds + " nodes=" + all.size() + " sum=" + total);
        System.out.println("ZS3CR bad_payload=" + bad[0] + " bad_hash=" + bad[1] + " weak_lost=" + weakLost);
        System.out.println(bad[0] == 0 && bad[1] == 0 && weakLost == 0 ? "ZS3CR ok" : "ZS3CR FAIL");
        if (sink == null) {
            System.out.println("unreachable");
        }
        System.err.println("ZS3CR cpu_ms_main=" + (mx.getCurrentThreadCpuTime() - cpu0) / 1_000_000);
    }
}
