// Marking-cycle stress for the G1 concurrent marker (the defect family the
// SteadyChurn recreation exposed indirectly): promotion churn + SATB edge
// churn + mid-cycle holder movement, tuned so concurrent marking, cleanup's
// in-place Old-region free, AND mixed collections all actually run.
//
// SteadyChurn/SteadyChurnLight at -Xmx16m stay YOUNG-ONLY on hosts where the
// young-pause cadence is a few thousand iterations (nodes die before they
// age to the promotion threshold, Old stays empty, IHOP never fires). This
// variant forces the cadence with a 64 KiB per-iteration temp array so ring
// slots survive >15 young pauses, promote, and then die IN OLD:
//
//   * ring of RING chain heads; one chain replaced per iteration → each
//     chain lives RING iterations ≈ 20+ young pauses → promotes, then dies;
//   * every iteration rewires two cross-chain `link` edges → steady SATB
//     pre-barrier traffic whose old targets are mostly Old objects;
//   * the walk() checksum touches a rotating window of chains every
//     iteration, so a freed-live Old object (the pre-fix cleanup defect)
//     surfaces as a wrong seq/zeroed payload within one ring turnover.
//
// Run (16m heap; IHOP lowered so marking cycles are frequent):
//
//   CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
//     cratonvm --nojit -XX:+UseG1GC -XX:InitiatingHeapOccupancyPercent=20 \
//       -Xmx16m -cp <classes> SteadyChurnPromote 30000
//
// Expected stdout: run the same class/args on HotSpot — the checksum is a
// deterministic pure function of the iteration count (no identity hashes,
// no timing), so HotSpot output is the golden value.
public final class SteadyChurnPromote {
    private static final int RING = 4096;
    private static final int CHAIN = 4;
    private static final int PAYLOAD = 48;
    private static final int TEMP = 64 * 1024;

    private static long sink;

    private static final class Node {
        Node next;
        Node link; // cross-chain edge, rewired every iteration (SATB churn)
        final int seq;
        final byte[] payload;

        Node(int seq) {
            this.seq = seq;
            this.payload = new byte[PAYLOAD];
            payload[0] = (byte) seq;
            payload[PAYLOAD - 1] = (byte) (seq >>> 8);
        }
    }

    private static Node chain(int base) {
        Node head = new Node(base);
        Node t = head;
        for (int j = 1; j < CHAIN; j++) {
            t.next = new Node(base + j);
            t = t.next;
        }
        return head;
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 30_000 : Integer.parseInt(args[0]);

        Node[] ring = new Node[RING];
        for (int i = 0; i < RING; i++) {
            ring[i] = chain(i * CHAIN);
        }

        long checksum = 0;
        for (int i = 0; i < iterations; i++) {
            int slot = i % RING;
            // Cut the dying chain's outgoing cross-links FIRST (each null
            // store fires the SATB pre-barrier — deleted-edge traffic), so
            // dead chains retain nothing: without this, era-N dead chains
            // transitively retain era-N-1 dead chains through their links —
            // unbounded BY CONSTRUCTION (a genuine workload leak, OOMs on
            // HotSpot too at small heaps).
            for (Node d = ring[slot]; d != null; d = d.next) {
                d.link = null;
            }
            // The replaced chain dies here. If it promoted (it survived ~20
            // young pauses), it dies IN OLD — cleanup/mixed must reclaim it.
            ring[slot] = chain((RING + i) * CHAIN);

            // SATB churn: rewire two cross-chain edges; the overwritten old
            // targets are the marker's snapshot-live obligations.
            int a = (i * 7 + 1) % RING;
            int b = (i * 13 + 5) % RING;
            ring[a].link = ring[b];
            ring[b].next.link = ring[a].next;

            // Young-pause pacing: big short-lived temp.
            byte[] temp = new byte[TEMP];
            temp[0] = (byte) i;
            temp[TEMP - 1] = (byte) (i >>> 16);
            sink += temp[0] + temp[TEMP - 1];

            // Verify a rotating window of 8 chains every iteration: a chain
            // whose Old region was freed while live shows up as a wrong seq
            // or a zeroed payload within one ring turnover.
            for (int k = 0; k < 8; k++) {
                Node n = ring[(i + k * (RING / 8)) % RING];
                int expectFirst = n.seq;
                int c = 0;
                while (n != null) {
                    if (n.seq != expectFirst + c || (n.payload[0] & 0xff) != (n.seq & 0xff)) {
                        throw new RuntimeException(
                                "corrupt chain at iter " + i + " k=" + k + " c=" + c);
                    }
                    checksum += (n.seq & 63) + (n.payload[PAYLOAD - 1] & 0xff);
                    c++;
                    n = n.next;
                }
                if (c != CHAIN) {
                    throw new RuntimeException("chain length " + c + " at iter " + i);
                }
            }
        }

        // Full-graph verification.
        for (int s = 0; s < RING; s++) {
            Node n = ring[s];
            int expectFirst = n.seq;
            int c = 0;
            while (n != null) {
                if (n.seq != expectFirst + c) {
                    throw new RuntimeException("corrupt final chain slot " + s);
                }
                checksum += n.seq & 7;
                c++;
                n = n.next;
            }
            if (c != CHAIN) {
                throw new RuntimeException("final chain length " + c + " slot " + s);
            }
        }

        keep(sink);
        System.out.println(checksum);
    }

    private static void keep(long v) {
        sink ^= v;
    }
}
