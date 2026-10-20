import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

/**
 * zs s1/sl: the three heap shapes the selective-relocation decision has to get
 * right, in one probe, so one row list A/Bs `CRATONVM_ZGC_SELECTIVE_RELOCATE`
 * unset / `=1` (z9 per-page filter) / `=2` (zs cycle gate).
 *
 * <ul>
 *   <li>{@code stream}: a dense retained block plus a garbage stream that keeps
 *       only its newest {@code keep} nodes (Zr7CmFixedWork's shape, where the
 *       default slide moves ~100 KB and rewrites the whole live set: both
 *       selective arms should decline those slides).</li>
 *   <li>{@code scatter}: {@code threads} threads each interleave a garbage
 *       stream with short runs of retained nodes, and the retained set GROWS
 *       (MtChurnProbe's shape, where the z9 per-page arm was +12 % CPU: the
 *       retained runs sit in long holes, so the per-page filter keeps every
 *       page and the allocator lives on recycled holes).</li>
 *   <li>{@code mosaic}: every eighth object of a long run is kept (fragmented
 *       garbage: every arm should slide).</li>
 * </ul>
 *
 * <p>stdout is one deterministic line per shape (a checksum over the retained
 * graph, recomputed by walking it after the churn) plus a final
 * {@code ZS1SL ok}; it must equal HotSpot's byte for byte. CPU time (all
 * threads, ThreadMXBean) goes to stderr only.
 *
 * <p>Usage: {@code Zs1SlSelectiveShapes <shape|all> [rounds] [threads]}
 */
public final class Zs1SlSelectiveShapes {

    static final class Node {
        Node next;
        Node far;
        final byte[] payload;
        final int tag;

        Node(Node next, int payloadBytes, int tag) {
            this.next = next;
            this.payload = new byte[payloadBytes];
            this.payload[0] = (byte) tag;
            this.tag = tag;
        }
    }

    static long walk(Node head) {
        long sum = 0;
        long n = 0;
        for (Node x = head; x != null; x = x.next) {
            sum = sum * 31 + x.tag + x.payload[0] + (x.far != null ? x.far.tag : 7);
            n++;
        }
        return sum ^ (n << 40);
    }

    /** Dense block + stream keeping its newest `keep` nodes. */
    static long stream(int rounds) {
        Node dense = null;
        for (int i = 0; i < 200_000; i++) {
            dense = new Node(dense, 48, i);
            if ((i & 63) == 0 && dense.next != null) {
                dense.far = dense.next;
            }
        }
        final int keep = 4096;
        Node[] ring = new Node[keep];
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 400_000; i++) {
                Node n = new Node(null, 96, i ^ r);
                ring[i & (keep - 1)] = n;
                sum += n.tag;
            }
        }
        Node head = null;
        for (int i = keep - 1; i >= 0; i--) {
            ring[i].next = head;
            head = ring[i];
        }
        return walk(dense) * 3 + walk(head) + sum;
    }

    /** Interleaved garbage + growing retained runs on `threads` threads. */
    static long scatter(int rounds, int threads) throws InterruptedException {
        final long[] sums = new long[threads];
        Thread[] ts = new Thread[threads];
        final int growPerRound = 80;
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                Node retained = null;
                long s = 0;
                for (int r = 0; r < rounds; r++) {
                    for (int i = 0; i < 4096; i++) {
                        Node dead = new Node(null, 224, i);
                        s += dead.payload.length + dead.tag;
                    }
                    for (int i = 0; i < growPerRound; i++) {
                        retained = new Node(retained, 224, r * 131 + i + id);
                        if ((i & 15) == 0 && retained.next != null) {
                            retained.far = retained.next;
                        }
                    }
                }
                sums[id] = s + walk(retained);
            }, "zs1sl-" + t);
            ts[t].start();
        }
        for (Thread th : ts) {
            th.join();
        }
        long sum = 0;
        for (long s : sums) {
            sum = sum * 17 + s;
        }
        return sum;
    }

    /** Every eighth object of a long run kept: fragmented garbage. */
    static long mosaic(int rounds) {
        Node kept = null;
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 200_000; i++) {
                Node n = new Node(null, 64, i + r);
                if ((i & 7) == 0) {
                    n.next = kept;
                    n.far = kept != null ? kept.next : null;
                    kept = n;
                } else {
                    sum += n.tag;
                }
            }
            // Drop the older half so the kept set stays bounded but scattered.
            int len = 0;
            for (Node x = kept; x != null; x = x.next) {
                len++;
            }
            Node x = kept;
            for (int i = 1; i < len / 2 && x != null; i++) {
                x = x.next;
            }
            if (x != null) {
                x.next = null;
            }
        }
        return walk(kept) + sum;
    }

    public static void main(String[] args) throws Exception {
        String shape = args.length > 0 ? args[0] : "all";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int threads = args.length > 2 ? Integer.parseInt(args[2]) : 4;
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        long cpu0 = totalCpu(mx);
        boolean all = shape.equals("all");
        if (all || shape.equals("stream")) {
            System.out.println("ZS1SL stream rounds=" + rounds + " sum=" + stream(rounds));
        }
        if (all || shape.equals("scatter")) {
            System.out.println("ZS1SL scatter rounds=" + rounds * 25 + " threads=" + threads
                    + " sum=" + scatter(rounds * 25, threads));
        }
        if (all || shape.equals("mosaic")) {
            System.out.println("ZS1SL mosaic rounds=" + rounds + " sum=" + mosaic(rounds));
        }
        System.out.println("ZS1SL ok");
        System.err.println("ZS1SL cpu_ms_main_and_workers=" + (totalCpu(mx) - cpu0) / 1_000_000L);
    }

    /** CPU of every live thread (the scatter workers have exited by now, so
     * this is a floor; use /usr/bin/time for the row's verdict). */
    static long totalCpu(ThreadMXBean mx) {
        long t = 0;
        for (long id : mx.getAllThreadIds()) {
            long c = mx.getThreadCpuTime(id);
            if (c > 0) {
                t += c;
            }
        }
        return t;
    }
}
