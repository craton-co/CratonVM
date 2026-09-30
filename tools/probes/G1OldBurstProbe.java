/**
 * A probe whose OLD generation grows, and whose reclaimable old garbage arrives
 * in BURSTS — built to falsify the mark-cycle back-off
 * ({@code CRATONVM_G1_IHOP_BACKOFF}).
 *
 * <p>WHY THIS EXISTS. The back-off was measured in wave 3 on
 * {@code G1ChurnPauseProbe} and looked excellent: mark cycles 8 → 1 on all nine
 * reps, marking work 8.7× less, −14.4% wall. The lane nevertheless refused to
 * flip its default, and the third of its three reasons is the one this probe
 * answers: <b>{@code G1ChurnPauseProbe} holds a FIXED retained set, so every
 * mark cycle on it is futile by construction.</b> Suppressing a cycle there can
 * never cost anything, because there was never anything for the cycle to find.
 * A workload on which the back-off cannot be wrong is not evidence that it is
 * right. See {@code docs/internal/g1-2026-09-20/w3c-the-six-w2c-flags-measured.md}
 * §3 and §5 — "the single highest-value run left in this round".
 *
 * <p>WHAT THE BACK-OFF ACTUALLY DOES, because the shape below is built against
 * the mechanism rather than against the description. After a mark cycle that
 * {@code G1Collector::note_mark_cycle_outcome} judges unproductive,
 * {@code check_ihop} additionally requires the old generation to have GROWN by
 * a width of regions before it will let another cycle start; the width DOUBLES
 * per unproductive cycle (capped at 64 regions) and snaps back to one region as
 * soon as a cycle is judged productive. So the back-off's gate is a
 * <i>growth</i> test, and its blind spot is any moment at which a lot of old
 * data DIES without the old generation growing — the gate stays shut exactly
 * when the cycle would have paid for itself.
 *
 * <p>THE SHAPE, and why each half of it is there. One "phase" is two halves:
 *
 * <ol>
 *   <li><b>Grow half.</b> Allocate a {@code burstMiB} disposable long-lived set
 *       AND a {@code coreDeltaMiB} permanent increment, interleaved in stripes
 *       (see {@code stripeNodes}), keeping both reachable and walking them so
 *       they age into the old generation. The old generation grows, so the gate
 *       opens and a mark cycle runs — and it finds almost nothing dead, because
 *       everything allocated in this half is still reachable. <b>These are the
 *       cycles it is FREE to suppress</b>, and they are what widens the gate.
 *   <li><b>Settle half.</b> Drop the whole burst — {@code burstMiB} of old data
 *       becomes garbage in one store — and then run a stream of pure YOUNG
 *       garbage that promotes nothing. Old occupancy is now high and <b>does
 *       not grow</b>. A mark cycle here is worth a large fraction of the heap:
 *       it is the only thing that can discover the burst is dead, free the old
 *       regions the burst filled, and give the mixed collector liveness data
 *       for the ones it shares with the core. <b>This is the cycle that must
 *       NOT be suppressed</b> — and it is precisely the one the growth gate,
 *       widened by the grow half, refuses.
 * </ol>
 *
 * <p>So a cycle suppressed at the wrong moment is punished (the dead burst
 * stays resident, the next grow half has nowhere to put its allocation, and the
 * collector discovers this by failing to evacuate —
 * {@code to_space_exhausted}), and a cycle suppressed at the right moment is
 * free. That is the discrimination the back-off claims to make, and neither
 * direction exists in {@code G1ChurnPauseProbe} at all.
 *
 * <p>THE CORE GROWS. {@code core} gains {@code coreDeltaMiB} every phase and is
 * never dropped, so the live set climbs monotonically across the run while the
 * garbage arrives in bursts. A live set that is flat makes the back-off
 * trivially right; one that grows monotonically with no garbage makes it
 * trivially wrong; this is the mixture, which is the only shape on which the
 * answer is a measurement rather than a definition.
 *
 * <p>{@code stripeNodes} IS THE INSTRUMENT, not a tuning knob. It is how many
 * consecutive nodes of one class (core, then burst) are allocated before
 * switching, and it controls how the burst's death is DISTRIBUTED over old
 * regions without changing how many bytes die. At a large stripe the burst
 * occupies whole regions, which cleanup frees in place; at a small stripe every
 * region is a mixture of core and burst, so no region reaches zero live bytes,
 * cleanup frees nothing, and the same reclamation has to come from a mixed
 * collection instead. The collector's progress test
 * ({@code note_mark_cycle_outcome}) reads only the first route, so sweeping
 * this argument holds the amount of real garbage constant while varying whether
 * the back-off can SEE it. A default of 64 nodes (16 KiB of nodes per stripe)
 * is the realistic middle: programs do not segregate their allocations by
 * lifetime, and they do not perfectly interleave them either.
 *
 * <p>THE CHECKSUM folds every node the probe creates (its tag and its payload
 * length) and every node it walks, plus a final fold over the surviving core,
 * so a run that is faster because an object was lost, not copied, or freed
 * while reachable produces a different number and fails rather than scores. It
 * is deterministic and independent of the collector: no identity hashes, no
 * timings, no iteration over anything whose order a GC could change.
 *
 * <p>Usage:
 * {@code G1OldBurstProbe <coreStartMiB> <coreDeltaMiB> <burstMiB> <phases> <youngChurnMiB> [stripeNodes]}
 * — defaults {@code 16 3 40 10 24 64}. Size it so
 * {@code coreStartMiB + phases*coreDeltaMiB + burstMiB} lands in the 50-70%
 * band of the heap: at the defaults that is 16 + 30 + 40 = 86 MiB of peak
 * reachable data, which wants {@code -Xmx160m}. With the back-off armed and
 * wrong, the dead bursts accumulate on top of that and the heap runs out.
 */
public final class G1OldBurstProbe {

    /** One node: a header, a ref, a tag and a payload. 256 bytes all in. */
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

    /** Nominal cost of one node including header and payload header. */
    static final int NODE_BYTES = 256;

    /** Payload bytes per node, leaving room for the two headers and the fields. */
    static final int PAYLOAD_BYTES = NODE_BYTES - 32;

    public static void main(String[] args) {
        int coreStartMiB = args.length > 0 ? Integer.parseInt(args[0]) : 16;
        int coreDeltaMiB = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        int burstMiB = args.length > 2 ? Integer.parseInt(args[2]) : 40;
        int phases = args.length > 3 ? Integer.parseInt(args[3]) : 10;
        int youngChurnMiB = args.length > 4 ? Integer.parseInt(args[4]) : 24;
        int stripeNodes = args.length > 5 ? Integer.parseInt(args[5]) : 64;
        if (stripeNodes < 1) {
            stripeNodes = 1;
        }

        final int coreStartNodes = (coreStartMiB * 1024 * 1024) / NODE_BYTES;
        final int coreDeltaNodes = (coreDeltaMiB * 1024 * 1024) / NODE_BYTES;
        final int burstNodes = (burstMiB * 1024 * 1024) / NODE_BYTES;
        final int youngChurnNodes = (youngChurnMiB * 1024 * 1024) / NODE_BYTES;

        long checksum = 0;
        long nodesAllocated = 0;
        long burstBytesDropped = 0;

        // The permanent set. A chain rather than an array so the marker has to
        // follow a real edge per node and the retained set is not one flat
        // object the card table can describe in a single entry.
        Node core = null;
        for (int i = 0; i < coreStartNodes; i++) {
            core = new Node(core, PAYLOAD_BYTES, i);
            checksum += core.tag + core.payload.length;
            nodesAllocated++;
        }

        long start = System.nanoTime();

        for (int phase = 0; phase < phases; phase++) {
            // ---- grow half -------------------------------------------------
            //
            // Both classes allocated in stripes from the SAME thread and the
            // same TLAB, so they land interleaved in address order and the old
            // regions they age into are mixtures. `burst` is a local chain:
            // dropping it at the end of this phase is a single store.
            Node burst = null;
            int burstMade = 0;
            int coreMade = 0;
            int tag = phase * 1_000_003;
            while (burstMade < burstNodes || coreMade < coreDeltaNodes) {
                for (int s = 0; s < stripeNodes && coreMade < coreDeltaNodes; s++, coreMade++) {
                    core = new Node(core, PAYLOAD_BYTES, tag + coreMade);
                    checksum += core.tag + core.payload.length;
                    nodesAllocated++;
                }
                for (int s = 0; s < stripeNodes && burstMade < burstNodes; s++, burstMade++) {
                    burst = new Node(burst, PAYLOAD_BYTES, tag + burstMade);
                    checksum += burst.tag + burst.payload.length;
                    nodesAllocated++;
                }
                // A little young garbage alongside, so young pauses happen
                // DURING the grow half and the two long-lived classes actually
                // age and get promoted rather than all arriving in old at once.
                for (int i = 0; i < stripeNodes * 4; i++) {
                    Node dead = new Node(null, PAYLOAD_BYTES, i);
                    checksum += dead.payload.length;
                    nodesAllocated++;
                }
            }

            // Touch both sets so they are genuinely reachable across the pauses
            // that follow, and so the reference-store barrier runs on receivers
            // that are already old.
            checksum += walk(core, 8192, phase);
            checksum += walk(burst, 8192, phase);

            // ---- settle half -----------------------------------------------
            //
            // THE DROP. `burstMiB` of old data becomes garbage here, in one
            // store, with no compensating allocation. Everything below this
            // line allocates young and retains nothing, so old occupancy stays
            // exactly where the grow half left it while a large fraction of it
            // is now dead. A mark cycle started in this window is worth
            // `burstMiB`; a mark cycle refused in this window costs it.
            burstBytesDropped += (long) burstNodes * NODE_BYTES;
            burst = null;

            for (int i = 0; i < youngChurnNodes; i++) {
                Node dead = new Node(null, PAYLOAD_BYTES, i);
                checksum += dead.payload.length;
                nodesAllocated++;
            }
            // Walk the core again: the survivors have to stay reachable through
            // the settle half too, or the "garbage arrives in bursts" claim
            // would be "everything dies at the phase boundary".
            checksum += walk(core, 8192, phase);
        }

        long wallMs = (System.nanoTime() - start) / 1_000_000L;

        // Final fold over everything that survived. A collector that freed a
        // reachable core node, or failed to fix up a reference into one it
        // moved, changes this number.
        long liveNodes = 0;
        long liveFold = 0;
        for (Node cur = core; cur != null; cur = cur.next) {
            liveFold += cur.tag + cur.payload.length;
            liveNodes++;
        }
        checksum += liveFold + liveNodes;

        System.out.println("G1OldBurstProbe"
                + " coreStart=" + coreStartMiB + "MiB"
                + " coreDelta=" + coreDeltaMiB + "MiB"
                + " burst=" + burstMiB + "MiB"
                + " phases=" + phases
                + " youngChurn=" + youngChurnMiB + "MiB"
                + " stripeNodes=" + stripeNodes
                + " liveNodes=" + liveNodes
                + " liveMiB=" + ((liveNodes * NODE_BYTES) / (1024 * 1024))
                + " nodesAllocated=" + nodesAllocated
                + " burstDroppedMiB=" + (burstBytesDropped / (1024 * 1024))
                + " wallMs=" + wallMs
                + " checksum=" + checksum);
    }

    /**
     * Walk up to {@code limit} nodes of a chain, mutating each tag so the write
     * barrier runs on an old receiver, and folding the result into a return
     * value the caller adds to the checksum. Bounded so the walk cost does not
     * grow with the core and turn this into a pointer-chasing benchmark.
     */
    static long walk(Node head, int limit, int phase) {
        long fold = 0;
        int walked = 0;
        Node cur = head;
        while (cur != null && walked < limit) {
            cur.tag = cur.tag + phase;
            fold += cur.tag;
            cur = cur.next;
            walked++;
        }
        return fold;
    }
}
