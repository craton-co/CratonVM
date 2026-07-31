/**
 * Multi-threaded allocation probe for the moving young generation.
 *
 * `BinTreesClassic` proves the single-threaded relocation contract but never
 * exercises the cross-thread obligation: `refresh_moving_young_coverage_for_collection`
 * treats a cycle as unproven whenever a PEER thread is in compiled code, because
 * a peer's registers and frame slots are not rewritable by this collection. This
 * probe puts several threads in long-lived compiled frames allocating hard enough
 * to trigger young collections from each of them, so that path is actually taken.
 *
 * The checksum is order-independent (a sum over per-thread sums), so it is
 * identical on HotSpot and CratonVM and identical across runs — any relocation
 * that strands or drops a live node changes it.
 */
public class MovingYoungConcurrentProbe {
    static final class Node {
        Node next;
        final int value;
        Node(Node next, int value) { this.next = next; this.value = value; }
    }

    /** Build and walk a fresh chain; keeps a live list across many allocations. */
    static long churn(int seed, int rounds, int len) {
        long sum = 0;
        Node retained = null;
        for (int r = 0; r < rounds; r++) {
            Node head = null;
            for (int i = 0; i < len; i++) {
                head = new Node(head, (seed + r + i) & 0xFFFF);
            }
            for (Node n = head; n != null; n = n.next) {
                sum += n.value;
            }
            // Keep one chain alive across the next round's allocations so a
            // young collection has real survivors to copy, and so a live
            // reference sits in a compiled frame slot while it runs.
            if ((r & 7) == 0) {
                retained = head;
            }
        }
        for (Node n = retained; n != null; n = n.next) {
            sum += n.value;
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int rounds  = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int len     = args.length > 2 ? Integer.parseInt(args[2]) : 2000;

        long[] results = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> results[id] = churn(id, rounds, len));
        }
        for (Thread th : ts) th.start();
        for (Thread th : ts) th.join();

        long total = 0;
        for (long r : results) total += r;
        System.out.println("r:threads=" + threads);
        System.out.println("r:checksum=" + total);
    }
}
