/**
 * gci i15/rs: the G1 young pause's base cost against a large, densely
 * cross-linked OLD graph that takes a SPARSE stream of old-to-young stores
 * (page gci-i2st-the-javac-stall-is-a-progressing-pause-regime-not-a-lost-wakeup-20261012,
 * G1 half; switch {@code CRATONVM_G1_YOUNG_CARD_SCREEN}).
 *
 * <p>The shape the 4-thread javac rows have, without javac: every old node
 * holds a far {@code link} to a pseudo-random other node (an Old -> Old edge
 * that crosses regions once the graph is promoted, so its card is dirty for
 * good), and each round stores a fresh young {@code Leaf} into every
 * {@code every}-th node (offset by the round), which makes EVERY old region a
 * remembered-set source of the young generation while only one node in
 * {@code every} actually points into it. A garbage stream between the stores
 * drives the young pauses. On the default path each young pause walks every
 * object of every old region twice (Phase 2 and the Phase-4 fix-up); HotSpot
 * G1 scans the cards that point into young.
 *
 * <p>Usage: {@code GciRs15YoungCardProbe [nodes] [rounds] [every] [garbage]}
 * (defaults 600000 200 64 200000). Stdout is one deterministic line (a pure
 * function of the arguments) for the HotSpot diff; the wall time goes to
 * stderr as {@code [rs15] ms=}.
 */
public final class GciRs15YoungCardProbe {
    static final class Node {
        Node link;
        Leaf young;
        final long v;

        Node(long v) {
            this.v = v;
        }
    }

    static final class Leaf {
        final long v;
        Leaf next;

        Leaf(long v) {
            this.v = v;
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 600_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int every = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        int garbage = args.length > 3 ? Integer.parseInt(args[3]) : 200_000;

        Node[] nodes = new Node[n];
        for (int i = 0; i < n; i++) {
            nodes[i] = new Node(i);
        }
        // Far links from a fixed LCG: the graph is a pure function of `n`.
        long s = 12345L;
        for (int i = 0; i < n; i++) {
            s = s * 6364136223846793005L + 1442695040888963407L;
            int j = (int) ((s >>> 33) % n);
            nodes[i].link = nodes[j];
        }

        long sum = 0L;
        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            int off = r % every;
            for (int i = off; i < n; i += every) {
                nodes[i].young = new Leaf((long) r * n + i);
            }
            Leaf g = null;
            for (int k = 0; k < garbage; k++) {
                Leaf x = new Leaf(k);
                x.next = (k & 7) == 0 ? null : g;
                g = x;
            }
            sum += g == null ? 0L : g.v;
            // Read a deterministic sample back through the old graph.
            for (int i = off; i < n; i += every * 16) {
                Node p = nodes[i].link;
                sum += p.v + (p.young != null ? p.young.v : -1L);
                sum += nodes[i].young.v;
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("GciRs15YoungCardProbe n=" + n + " rounds=" + rounds + " every=" + every
                + " garbage=" + garbage + " checksum=" + sum);
        System.err.println("[rs15] ms=" + ms);
    }
}
