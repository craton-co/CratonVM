/**
 * A RECONSTRUCTION of the `SteadyChurn` shape the 2026 parallel-evacuator race
 * note names as its repro arm.
 *
 * <h2>Why a reconstruction, and why it says so in its name</h2>
 *
 * <p>`docs/internal/g1-2026-09-20/lane-d-mixed-is-serial-for-a-race-the-young-path-runs-anyway.md`
 * says mixed evacuation refuses the parallel evacuator because of "a RARE,
 * NON-DETERMINISTIC race … reproduced on {@code SteadyChurn @16m --nojit}
 * (~1 in 8 runs, smallest heap only)". That workload is not in the tree: the
 * archived bug note
 * ({@code docs/internal/fixed-suite-bugs/g1-parallel-evac-persistent-forwarding-root-remap.md})
 * runs it from {@code scratch/g1par}, which is not tracked, and records its
 * expected output as a bare constant with no source beside it. So the recorded
 * arm cannot be re-run, which is a large part of why the refusal has stayed an
 * opinion for months.
 *
 * <p>This file is built from that note's own DESCRIPTION of the workload —
 * "keeps a fixed 4096-node linked list + heavy young churn" — and it is named
 * a recreation because its checksum is its own, not the historical
 * {@code 2002062093760}. Pretending otherwise would let a future reader
 * conclude that the original arm had been re-run when it had not.
 *
 * <h2>The mechanism the note is about, stated so a run can be read</h2>
 *
 * <p>The archived note's own diagnosis of why this shape and this heap size:
 * "at 16m the parallel evacuator exhausts to-space and self-forwards where
 * serial (which packs into partially-filled survivors) does not — hence
 * parallel-only and small-heap-only". So the discriminating property is a
 * retained set large enough, relative to the heap, that the Survivor
 * destination runs out mid-pause. A bigger heap does not exercise it; neither
 * does a workload with no retained set.
 *
 * <h2>Reading a run</h2>
 *
 * <p>The checksum is printed and is deterministic for a given
 * {@code iterations}: a run that differs has LOST or DUPLICATED an object,
 * which is the failure the race note describes (the historical signature was a
 * {@code java/lang/Object} throw, i.e. a zeroed header read back out of a
 * recycled region). Run the arms at several {@code CRATONVM_G1_WORKERS}
 * values, and read the note's own discriminator first: a divergence that
 * SURVIVES {@code CRATONVM_G1_WORKERS=1} is not a race at all — it is a logic
 * divergence between the two arms, which is what defect G1-9 turned out to be
 * after being chased as a race for months.
 *
 * <p>Usage: {@code SteadyChurnRecreation [iterations] [liveNodes]}
 */
public final class SteadyChurnRecreation {

    /** One node of the fixed retained list. */
    static final class Node {
        Node next;
        long tag;
        /** Some body, so the retained set is bytes and not just headers. */
        final long[] body;

        Node(Node next, long tag, int bodyLongs) {
            this.next = next;
            this.tag = tag;
            this.body = new long[bodyLongs];
            this.body[0] = tag;
        }
    }

    public static void main(String[] args) {
        long iterations = args.length > 0 ? Long.parseLong(args[0]) : 2_000_000L;
        int liveNodes = args.length > 1 ? Integer.parseInt(args[1]) : 4096;

        // The fixed retained list. Built once and never dropped, so every pause
        // has real survivors to place and the Survivor destination is what runs
        // out — the property the archived note says makes this shape
        // discriminating at a small heap and not at a large one.
        Node head = null;
        for (int i = 0; i < liveNodes; i++) {
            head = new Node(head, i, 24);
        }

        long checksum = 0;
        Node cursor = head;
        for (long it = 0; it < iterations; it++) {
            // The young churn. Short-lived, escapes far enough that it is not
            // folded away, and dies before the next pause.
            long[] garbage = new long[8];
            garbage[0] = it;
            garbage[7] = it * 31L;
            checksum += garbage[0] + garbage[7];

            // Touch the retained list, so its nodes are genuinely reachable at
            // every pause and the reference-store barrier runs on a retained
            // receiver. Walking one node per iteration keeps the mutator cost
            // flat while covering the whole list many times over.
            if (cursor == null) {
                cursor = head;
            }
            cursor.tag += 1;
            checksum += cursor.tag & 0xFFL;
            cursor = cursor.next;
        }

        // Read the whole retained list at the end: a lost or duplicated node
        // shows up here rather than being quietly tolerated.
        long alive = 0;
        int seen = 0;
        for (Node n = head; n != null; n = n.next) {
            alive += n.tag + n.body[0];
            seen++;
        }

        System.out.println("SteadyChurnRecreation"
                + " iterations=" + iterations
                + " liveNodes=" + liveNodes
                + " seen=" + seen
                + " alive=" + alive
                + " checksum=" + checksum);
    }
}
