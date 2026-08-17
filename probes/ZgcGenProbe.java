// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The workload a generational collector exists for: a stable live set plus a
 * high churn rate, both reached by ALLOCATING rather than by calling
 * {@code System.gc()}.
 *
 * <h2>Why the shape is what it is</h2>
 *
 * <p>Phase G's claim is that a young cycle does strictly less work than a full
 * one. That claim is only measurable on a workload with a large live set the
 * cycle can SKIP and a lot of garbage it must still reclaim. {@code BigLive}
 * has the first half and none of the second; {@code ZgcConcMarkProbe} has the
 * second and a live set that turns over. This has both:
 *
 * <ul>
 *   <li>a retained matrix of {@code RETAINED} linked nodes -- the old
 *       generation once it has survived the promotion age;
 *   <li>{@code ROUNDS} rounds each allocating {@code CHURN} short-lived nodes
 *       that are dropped immediately.
 * </ul>
 *
 * <p>The retained half also has its references REWRITTEN each round, into
 * freshly allocated objects. That is deliberate and it is the part a card
 * barrier is measured by: without those stores the remembered set stays empty
 * and a young cycle would be trivially correct with no cards at all, which is
 * exactly the vacuous configuration this probe must not be.
 *
 * <h2>The reachability check is a SELF-TAG, not a survivor count</h2>
 *
 * <p>An object freed while still referenced leaves the reference intact and the
 * memory zeroed, so counting survivors reads the bug as a pass. Slot 0 of every
 * retained node points at the node itself, so a node that was reclaimed and
 * zeroed reads {@code null} there. {@code BAD} is the count of those and it is
 * the only line that decides correctness.
 */
public final class ZgcGenProbe {

    /** One node: a self-tag, a chain link, and a slot the churn rewrites. */
    static final class Node {
        Node self;
        Node next;
        Node written;
        int payload;
    }

    public static void main(String[] args) throws Exception {
        int retained = intArg(args, 0, 120_000);
        int churn = intArg(args, 1, 20_000);
        int rounds = intArg(args, 2, 400);

        System.out.println("ZgcGenProbe retained=" + retained
                + " churn=" + churn + " rounds=" + rounds);

        // ---- the old generation ---------------------------------------
        Node[] keep = new Node[retained];
        for (int i = 0; i < retained; i++) {
            Node n = new Node();
            n.self = n;              // the tag
            n.payload = i;
            keep[i] = n;
        }
        for (int i = 0; i < retained - 1; i++) {
            keep[i].next = keep[i + 1];
        }

        long t0 = System.nanoTime();
        long allocated = 0;

        // ---- churn, with old-to-young stores ---------------------------
        for (int r = 0; r < rounds; r++) {
            // Short-lived garbage: allocated, linked into a local chain so the
            // JIT cannot elide it, then dropped.
            Node head = null;
            for (int i = 0; i < churn; i++) {
                Node n = new Node();
                n.self = n;
                n.payload = i;
                n.next = head;
                head = n;
            }
            if (head.payload < -1) {
                throw new IllegalStateException("unreachable, keeps head live");
            }
            allocated += churn;

            // OLD-TO-YOUNG STORES. Every 16th retained node gets a fresh young
            // object written into it. This is the edge the card barrier has to
            // record and the young cycle has to follow; without it the
            // remembered set is empty and the measurement is vacuous.
            for (int i = r % 16; i < retained; i += 16) {
                Node fresh = new Node();
                fresh.self = fresh;
                fresh.payload = r;
                keep[i].written = fresh;
                allocated++;
            }
        }
        long wallMs = (System.nanoTime() - t0) / 1_000_000L;

        // ---- the verdict ----------------------------------------------
        int bad = 0;
        int chain = 0;
        int written = 0;
        for (int i = 0; i < retained; i++) {
            Node n = keep[i];
            if (n == null || n.self != n || n.payload != i) {
                bad++;
                continue;
            }
            if (n.next != null) {
                chain++;
            }
            // A written slot must ALSO still be intact: it is the object that
            // was only ever reachable through an old-generation field, which is
            // the exact object a missing card frees.
            Node w = n.written;
            if (w != null) {
                if (w.self != w) {
                    bad++;
                } else {
                    written++;
                }
            }
        }

        System.out.println("ZgcGenProbe done wall_ms=" + wallMs
                + " allocated=" + allocated
                + " retained=" + retained
                + " chain_links=" + chain
                + " written_intact=" + written);
        System.out.println("ZgcGenProbe BAD=" + bad + (bad == 0 ? " OK" : " CORRUPT"));
        if (bad != 0) {
            throw new IllegalStateException("reachable objects were reclaimed: " + bad);
        }
        if (written == 0) {
            // Not a correctness failure, but the run proves nothing about the
            // remembered set, so say so rather than let it read as a pass.
            System.out.println("ZgcGenProbe WARNING written_intact=0 -- no "
                    + "old-to-young edge survived to the end; this run is not "
                    + "evidence about the card barrier");
        }
    }

    private static int intArg(String[] args, int i, int dflt) {
        if (args.length <= i) {
            return dflt;
        }
        return Integer.parseInt(args[i]);
    }
}
