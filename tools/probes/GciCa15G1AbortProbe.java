import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.util.BitSet;
import java.util.TreeMap;

/**
 * gci i15/ca: a G1 full collection that ABORTS the open concurrent cycle
 * instead of finishing it (opt-in CRATONVM_G1_FULL_GC_ABORTS_OPEN_CYCLE;
 * page docs/internal/gc/gci-i13oa-g1-allocation-slow-path-and-oome-review-findings-RETIRED-UNFIXED-20261018.md
 * item 7's cost half), against HotSpot 25 {@code -XX:+UseG1GC}.
 *
 * <p>Both modes keep a live old graph of {@code chains} singly linked chains
 * whose node values are unique, and run a MUTATOR thread that rewires it
 * without allocating (swaps two chain heads in the slot array, or exchanges
 * the tails of two chains), so every SATB-logged overwrite of the aborted
 * cycle is one the fresh cycle must not need. After the mutator is joined,
 * the graph is walked: every value seen exactly once and the node count
 * exact, or {@code intact false} (a node freed while reachable).
 *
 * <ul>
 *   <li>{@code sysgc}: 16384 chains of 128 nodes (about half of -Xmx128m,
 *       above the default IHOP), then 40 rounds of about 8 MB of churn and ONE
 *       {@code System.gc()}: a call often finds a cycle marking.</li>
 *   <li>{@code oome}: 4096 chains of 64 nodes, then three fills to
 *       {@code OutOfMemoryError} (each drops its fill): the allocation-failure
 *       ladder's full collection, often with a cycle open.</li>
 *   <li>{@code overlap} (gci i16/ca): 16384 chains of 96 nodes, a churn
 *       thread filling Eden, and 40 {@code System.gc()} calls, each made right
 *       after a young pause that left the old pool at or above 45% of the max
 *       heap (a cycle was just started by allocation pressure). stdout
 *       {@code overlap rounds 40} / {@code overlap graph intact true}; stderr
 *       adds {@code candidates=} (calls made that way) and {@code timed_out=}.</li>
 * </ul>
 *
 * <p>stdout (deterministic; HotSpot 25 G1):
 * <pre>
 * mode sysgc
 * sysgc rounds 40
 * sysgc graph intact true
 * PROBE-OK
 * </pre>
 * ({@code oome}: {@code oome round 1 Java heap space} .. {@code round 3},
 * {@code oome graph intact true}.) stderr: {@code [gci-ca15] <mode> ms=<wall
 * ms of the loop> swaps=<mutator rounds> deltas=<bean count deltas>}.
 */
public class GciCa15G1AbortProbe {
    static final class Node {
        Node next;
        final int v;

        Node(int v) {
            this.v = v;
        }
    }

    static final class FillNode {
        final long[] data = new long[126];
        FillNode next;
    }

    static volatile Object sink;
    static volatile boolean stop;
    static volatile long swaps;

    static Node[] build(int chains, int len) {
        Node[] slots = new Node[chains];
        for (int c = 0; c < chains; c++) {
            Node head = null;
            for (int k = len - 1; k >= 0; k--) {
                Node n = new Node(c * len + k);
                n.next = head;
                head = n;
            }
            slots[c] = head;
        }
        return slots;
    }

    /** Every value in [0, total) seen exactly once, and nothing else. */
    static boolean intact(Node[] slots, int total) {
        BitSet seen = new BitSet(total);
        int count = 0;
        for (Node head : slots) {
            for (Node at = head; at != null; at = at.next) {
                int v = at.v;
                if (v < 0 || v >= total || seen.get(v)) {
                    return false;
                }
                seen.set(v);
                count++;
                if (count > total) {
                    return false;
                }
            }
        }
        return count == total;
    }

    /** Rewires the graph until {@link #stop}; allocates nothing in its loop. */
    static Thread mutator(Node[] slots, long seed) {
        Thread m = new Thread(() -> {
            long x = seed;
            int n = slots.length;
            long done = 0;
            while (!stop) {
                for (int r = 0; r < 1024; r++) {
                    x = x * 6364136223846793005L + 1442695040888963407L;
                    int i = (int) ((x >>> 33) % n);
                    x = x * 6364136223846793005L + 1442695040888963407L;
                    int j = (int) ((x >>> 33) % n);
                    if (i == j) {
                        continue;
                    }
                    Node a = slots[i];
                    Node b = slots[j];
                    if ((r & 1) == 0) {
                        slots[i] = b;
                        slots[j] = a;
                    } else {
                        Node tail = a.next;
                        a.next = b.next;
                        b.next = tail;
                    }
                }
                done++;
                Thread.yield();
            }
            swaps = done;
        }, "ca15-mutator");
        m.setDaemon(true);
        m.start();
        return m;
    }

    static void churn() {
        Object[] window = new Object[256];
        for (int i = 0; i < 32 * 1024; i++) {
            window[i % window.length] = new byte[256];
        }
        sink = window;
    }

    /** The error's message (an existing String); the fill dies with this frame. */
    static String fillUntilOome() {
        FillNode h = null;
        try {
            while (true) {
                FillNode f = new FillNode();
                f.next = h;
                h = f;
            }
        } catch (OutOfMemoryError e) {
            h = null;
            return e.getMessage();
        }
    }

    static TreeMap<String, Long> counts() {
        TreeMap<String, Long> m = new TreeMap<>();
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            m.put(b.getName(), b.getCollectionCount());
        }
        return m;
    }

    static String deltas(TreeMap<String, Long> before, TreeMap<String, Long> after) {
        StringBuilder sb = new StringBuilder();
        for (String name : after.keySet()) {
            Long b = before.get(name);
            long d = after.get(name) - (b == null ? 0L : b.longValue());
            sb.append(' ').append(name.replace(' ', '_')).append('=').append(d);
        }
        return sb.toString();
    }

    static void sysgc() throws InterruptedException {
        final int chains = 16384;
        final int len = 128;
        Node[] slots = build(chains, len);
        Thread m = mutator(slots, 15L);
        TreeMap<String, Long> before = counts();
        long t0 = System.nanoTime();
        for (int round = 0; round < 40; round++) {
            churn();
            System.gc();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        stop = true;
        m.join();
        sink = null;
        boolean ok = intact(slots, chains * len);
        System.out.println("sysgc rounds 40");
        System.out.println("sysgc graph intact " + ok);
        System.err.println("[gci-ca15] sysgc ms=" + ms + " swaps=" + swaps + " deltas="
                + deltas(before, counts()));
    }

    static volatile boolean stopChurn;

    /** Allocation pressure on its own thread: fills Eden so young pauses (and IHOP starts) keep coming. */
    static Thread churner() {
        Thread c = new Thread(() -> {
            Object[] window = new Object[1024];
            int i = 0;
            while (!stopChurn) {
                window[i] = new byte[1024];
                i = (i + 1) & 1023;
            }
            sink = window;
        }, "ca15-churn");
        c.setDaemon(true);
        c.start();
        return c;
    }

    static GarbageCollectorMXBean collector(String name) {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().equals(name)) {
                return b;
            }
        }
        return null;
    }

    static MemoryPoolMXBean pool(String name) {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals(name)) {
                return p;
            }
        }
        return null;
    }

    /**
     * gci i16/ca: System.gc() while a cycle that allocation pressure started is
     * still marking. Before each call (G1 only: both beans present), wait up to
     * 2 s for a young pause since the previous call that left the old pool at or
     * above 45% of the max heap (the default IHOP: that pause opens a cycle), and
     * call at once; `candidates` counts the calls made that way.
     */
    static void overlap() throws InterruptedException {
        final int chains = 16384;
        final int len = 96;
        Node[] slots = build(chains, len);
        Thread m = mutator(slots, 21L);
        Thread c = churner();
        GarbageCollectorMXBean young = collector("G1 Young Generation");
        MemoryPoolMXBean old = pool("G1 Old Gen");
        long ihop = Runtime.getRuntime().maxMemory() / 100L * 45L;
        TreeMap<String, Long> before = counts();
        int candidates = 0;
        int timedOut = 0;
        long t0 = System.nanoTime();
        for (int round = 0; round < 40; round++) {
            if (young != null && old != null) {
                long seen = young.getCollectionCount();
                long deadline = System.nanoTime() + 2_000_000_000L;
                boolean hit = false;
                while (System.nanoTime() < deadline) {
                    if (young.getCollectionCount() > seen && old.getUsage().getUsed() >= ihop) {
                        hit = true;
                        break;
                    }
                    Thread.sleep(1);
                }
                if (hit) {
                    candidates++;
                } else {
                    timedOut++;
                }
            }
            System.gc();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        stopChurn = true;
        stop = true;
        c.join();
        m.join();
        sink = null;
        boolean ok = intact(slots, chains * len);
        System.out.println("overlap rounds 40");
        System.out.println("overlap graph intact " + ok);
        System.err.println("[gci-ca15] overlap ms=" + ms + " swaps=" + swaps + " candidates=" + candidates
                + " timed_out=" + timedOut + " deltas=" + deltas(before, counts()));
    }

    static void oome() throws InterruptedException {
        final int chains = 4096;
        final int len = 64;
        Node[] slots = build(chains, len);
        Thread m = mutator(slots, 7L);
        TreeMap<String, Long> before = counts();
        String[] msgs = new String[3];
        long t0 = System.nanoTime();
        for (int r = 0; r < 3; r++) {
            msgs[r] = fillUntilOome();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        stop = true;
        m.join();
        for (int r = 0; r < 3; r++) {
            System.out.println("oome round " + (r + 1) + " " + msgs[r]);
        }
        System.out.println("oome graph intact " + intact(slots, chains * len));
        System.err.println("[gci-ca15] oome ms=" + ms + " swaps=" + swaps + " deltas="
                + deltas(before, counts()));
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "sysgc";
        System.out.println("mode " + mode);
        switch (mode) {
            case "sysgc":
                sysgc();
                break;
            case "oome":
                oome();
                break;
            case "overlap":
                overlap();
                break;
            default:
                System.out.println("unknown mode " + mode);
                return;
        }
        System.out.println("PROBE-OK");
    }
}
