/**
 * G1AUD-9 measurement probe: young-pause cost and collection frequency under a
 * steady allocation churn with a fixed live set.
 *
 * <p>The shape the audit's two headline changes are for:
 *
 * <ul>
 *   <li>a live set that is RETAINED (so every pause has real survivors to copy,
 *       which is what makes the per-object evacuation-destination scan visible
 *       at all — a pause that copies nothing pays for no destinations);
 *   <li>a much larger garbage stream around it (so regions are recycled
 *       constantly, which is what makes the region-reset scrub and the Free
 *       search visible);
 *   <li>no I/O, no JIT-heavy inner loop, and its own control (`--noalloc`) so a
 *       reader can subtract the interpreter's own cost rather than attributing
 *       the whole wall to the collector.
 * </ul>
 *
 * <p>Usage: {@code G1ChurnPauseProbe <liveMiB> <churnRounds> [noalloc]}
 */
public final class G1ChurnPauseProbe {

    /** One node of the retained live set: a header, a ref, and some payload. */
    static final class Node {
        Node next;
        final byte[] payload;
        int tag;

        Node(Node next, int payloadBytes, int tag) {
            this.next = next;
            this.payload = new byte[payloadBytes];
            this.tag = tag;
        }
    }

    public static void main(String[] args) {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 24;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        boolean noalloc = args.length > 2 && args[2].equals("noalloc");

        final int nodeBytes = 256;
        final int liveNodes = (liveMiB * 1024 * 1024) / nodeBytes;

        // The retained set. Built once and kept for the whole run, so every
        // pause has to copy it (or promote it) rather than free it.
        Node head = null;
        for (int i = 0; i < liveNodes; i++) {
            head = new Node(head, nodeBytes - 32, i);
        }

        long checksum = 0;
        long garbageBytes = 0;
        long start = System.nanoTime();

        for (int r = 0; r < rounds; r++) {
            if (!noalloc) {
                // The garbage stream: short-lived nodes that die before the
                // next pause. 4 MiB per round.
                for (int i = 0; i < (4 * 1024 * 1024) / nodeBytes; i++) {
                    Node dead = new Node(null, nodeBytes - 32, i);
                    checksum += dead.payload.length + dead.tag;
                    garbageBytes += nodeBytes;
                }
            }
            // Touch the live set so it stays genuinely reachable and the
            // reference-store barrier runs on retained receivers.
            Node cur = head;
            int walked = 0;
            while (cur != null && walked < 4096) {
                cur.tag = cur.tag + r;
                checksum += cur.tag;
                cur = cur.next;
                walked++;
            }
        }

        long wallMs = (System.nanoTime() - start) / 1_000_000L;
        System.out.println("G1ChurnPauseProbe"
                + " live=" + liveMiB + "MiB"
                + " liveNodes=" + liveNodes
                + " rounds=" + rounds
                + " noalloc=" + noalloc
                + " garbageMiB=" + (garbageBytes / (1024 * 1024))
                + " wallMs=" + wallMs
                + " checksum=" + checksum);
    }
}
