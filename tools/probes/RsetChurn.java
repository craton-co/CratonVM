/**
 * G1 remembered-set measurement probe: a live set that generates load-bearing
 * OLD to YOUNG edges, continuously, so `rset_bytes_per_live_byte` has something
 * to measure.
 *
 * <h2>Why this file exists (twice over)</h2>
 *
 * <p>The G1 audit's §9 item 5 records a measurement —
 * {@code rset_bytes_per_live_byte = 0.000034}, 176 bytes of remembered set
 * against 5,113,280 bytes live — and concludes from it that region-granular
 * remembered sets need not be replaced by a card table. The commit that
 * recorded it says, in as many words, "The probe is in the tree:
 * {@code apps/g1_probe/RsetChurn.java}, because the last finding of this kind
 * named a probe that was not". That commit touched no {@code .java} file, and
 * could not have: {@code apps/} is in {@code .gitignore}, so the {@code git add}
 * added nothing and said nothing. The number has not been reproducible since the
 * day it was taken. This file lives in {@code probes/}, which is tracked.
 *
 * <p>This is that probe, reconstructed from the audit's own description of it.
 * It matters now because F-05 revisits that conclusion: the measurement answers
 * the SPACE question (how many bytes of remembered set per byte of live data)
 * and was read as also answering the TIME one (what it costs to ACT on an
 * entry, which for a region-granular set is a walk of the whole source region).
 * Re-deciding that needs the instrument back.
 *
 * <h2>The shape, and why each part of it</h2>
 *
 * <ul>
 *   <li><b>Retained trees</b>, built once and kept, so their nodes age into the
 *       Old generation and stay there. An rset entry is only interesting when
 *       its source is old — a young-to-young edge needs no entry, because young
 *       is always collected whole.
 *   <li><b>Leaves re-pointed at FRESH arrays every round</b>, so the edges are
 *       old-to-young and are created by the mutator write barrier rather than
 *       by the collector. That is the traffic the remembered set exists for.
 *   <li><b>Enough rounds to complete a concurrent mark cycle</b>: the rset
 *       gauge is published from {@code cleanup}, so a run that never crosses
 *       IHOP publishes nothing and reports a confident zero. The audit records
 *       three separate confident zeros reached this way, one of them from a
 *       probe whose live set never aged into Old at all.
 * </ul>
 *
 * <h2>Running it</h2>
 *
 * <pre>{@code
 * cratonvm -XX:+UseG1GC -Xmx16m -XX:InitiatingHeapOccupancyPercent=15 \
 *          --nojit -c . RsetChurn 12 300
 * }</pre>
 *
 * <p>with {@code CRATONVM_GC_STATS=1}. Heap size decides whether this works at
 * all: the audit found 32/24/20 MiB produced 54 young pauses and NO mark cycle
 * (so {@code rset_bytes=0}), and 16 MiB produced 323 cycles — 85 young, 158
 * mixed, and the concurrent cleanups that publish the gauge. Check that a mark
 * cycle actually ran before believing any number this run produces.
 *
 * <p>The checksum is printed so a run can be diffed against a real JDK's.
 */
public final class RsetChurn {

    /** A binary tree node. The two child references are the retained edges. */
    static final class Node {
        Node left;
        Node right;
        /** Re-pointed at a fresh young array every round, on leaves only. */
        Object young;
        int tag;

        Node(int tag) {
            this.tag = tag;
        }
    }

    private static Node build(int depth, int tag) {
        Node n = new Node(tag);
        if (depth > 0) {
            n.left = build(depth - 1, tag * 2 + 1);
            n.right = build(depth - 1, tag * 2 + 2);
        }
        return n;
    }

    /** Re-point every leaf at a freshly allocated array; returns leaves seen. */
    private static long repoint(Node n, int round, long[] checksum) {
        if (n == null) {
            return 0;
        }
        if (n.left == null && n.right == null) {
            // The load-bearing store: an OLD leaf (after a few collections)
            // takes a reference to a YOUNG array. Every one of these runs the
            // remembered-set post-write barrier.
            int[] fresh = new int[8];
            fresh[0] = n.tag + round;
            n.young = fresh;
            checksum[0] += fresh[0];
            return 1;
        }
        return repoint(n.left, round, checksum) + repoint(n.right, round, checksum);
    }

    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 300;

        // Four trees, so the edges are spread over several source regions
        // rather than concentrating in one — a single source region would make
        // the rset one entry wide no matter how many edges it holds, which is
        // the region-granularity property under examination.
        final int trees = 4;
        Node[] roots = new Node[trees];
        for (int t = 0; t < trees; t++) {
            roots[t] = build(depth, t);
        }

        long[] checksum = new long[1];
        long leaves = 0;
        long start = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            for (int t = 0; t < trees; t++) {
                leaves += repoint(roots[t], r, checksum);
            }
        }
        long wallMs = (System.nanoTime() - start) / 1_000_000L;

        // Keep the trees reachable to the very end, so nothing above is
        // optimised into a dead store and the live set is genuinely live at the
        // last collection.
        long alive = 0;
        for (int t = 0; t < trees; t++) {
            alive += roots[t].tag;
        }

        System.out.println("RsetChurn"
                + " depth=" + depth
                + " trees=" + trees
                + " rounds=" + rounds
                + " leafStores=" + leaves
                + " wallMs=" + wallMs
                + " alive=" + alive
                + " checksum=" + checksum[0]);
    }
}
