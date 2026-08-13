/**
 * Reach the three young-from-space walks that no suite workload enters:
 * `mark_young_to_old_refs` and `fixup_young_old_refs` (both on the major
 * mark-compact path) and `walk_young_objects` (the diagnostic walk).
 *
 * The three ingredients, because each gates a different one:
 *   1. a lot of `new Object()` — an empty object is a wholly zero 16-byte
 *      header, which is the shape the walks misread as a desync;
 *   2. a surviving set with young->old references, so the major GC's
 *      young-to-old seed has something to find and the compaction has
 *      something to fix up;
 *   3. `System.gc()`, which CratonVM turns into an explicit major request
 *      (`request_major_gc`) rather than waiting for old gen to reach 75%.
 *
 * Deliberately allocation-shaped rather than time-shaped: the interesting
 * thing is the number of major cycles, not the wall clock.
 */
public class GcWalkProbe {

    /** Long-lived holders: these age, get promoted, and end up old->young. */
    static Node[] kept = new Node[1 << 12];

    static final class Node {
        Object payload;   // points at an empty object
        Node next;        // young->young or young->old once promoted
        int tag;
        Node(Object p, Node n, int t) { payload = p; next = n; tag = t; }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        int perRound = args.length > 1 ? Integer.parseInt(args[1]) : 300_000;

        long live = 0;
        for (int round = 0; round < rounds; round++) {
            for (int i = 0; i < perRound; i++) {
                // The shape under test: a bare `new Object()` has class_id 0,
                // shape 0 and a zero mark word, i.e. 16 all-zero bytes.
                Object empty = new Object();
                if ((i & 15) == 0) {
                    int slot = (i >>> 4) & (kept.length - 1);
                    // Chain onto the previous occupant so the surviving graph
                    // has real cross-object edges for the fixup pass to rewrite.
                    kept[slot] = new Node(empty, kept[slot], i);
                } else if ((i & 3) == 0) {
                    // Short-lived holder: dies immediately, leaving its empty
                    // payload dead too — this is what makes the zero RUNS.
                    Node tmp = new Node(empty, null, i);
                    if (tmp.tag < 0) throw new IllegalStateException();
                }
            }
            // Trim half the chains so the major cycle has old-gen garbage to
            // reclaim and compact around, rather than a monotonically growing
            // live set.
            for (int s = 0; s < kept.length; s += 2) {
                Node n = kept[s];
                if (n != null) kept[s] = n.next;
            }
            System.gc();
            live = 0;
            for (Node n : kept) for (Node p = n; p != null; p = p.next) live++;
            System.out.println("round=" + round + " liveNodes=" + live);
        }
        System.out.println("PROBE-DONE liveNodes=" + live);
    }
}
