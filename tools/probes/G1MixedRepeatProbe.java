/**
 * A workload that takes MIXED PAUSES, repeatedly, and copies real bytes in each
 * one.
 *
 * <p>WHY THIS EXISTS. Four waves of this round measured the mixed path with no
 * denominator under any of it.
 * {@code docs/internal/g1-2026-09-20/README.md} §4:
 *
 * <blockquote>Eight {@code G1OldBurstProbe} configurations plus the standing
 * battery: the runs that complete produce <b>zero</b> mixed pauses in up to
 * 8&nbsp;945 young pauses, and the one configuration that reaches a mixed pause
 * does so only by exhausting the heap — whereupon that pause copies zero bytes
 * and teaches nothing.</blockquote>
 *
 * <p>Three separate preconditions have to hold at once before G1 will take a
 * mixed pause, and the existing probes satisfy at most two. Each section below
 * is one of them, and the probe's shape is the conjunction.
 *
 * <h2>1. A mark cycle must COMPLETE</h2>
 *
 * {@code marking_complete} is set only by {@code cleanup}, and the only
 * production caller of the start/finish pair is {@code maybe_concurrent_gc},
 * which runs in {@code maybe_gc}'s epilogue — i.e. only on a pause that
 * {@code maybe_gc} itself took. G1 triggers most of its young pauses from
 * inside the allocator instead, and a JIT-compiled allocation loop reaches
 * {@code maybe_gc} zero times. So the gate is usually never armed, and no
 * amount of old-generation shaping can matter while that is true. The probe
 * does not try to fix this from Java; the RECIPE carries the flag
 * ({@code CRATONVM_G1_JIT_MARK_DRIVER=1}, or {@code --nojit}), and the
 * {@code forceCycleEvery} argument is a third, flag-free route — a
 * {@code System.gc()} every N phases, which reaches
 * {@code maybe_gc_forced} → {@code maybe_concurrent_gc} → {@code
 * last_ditch_reclaim}. Default 0 = off, so the default arm measures the
 * collector rather than the probe.
 *
 * <h2>2. Old regions must be PARTLY dead, not wholly dead and not wholly live</h2>
 *
 * This is the part every earlier probe got wrong, in one direction or the
 * other, and it is why the obvious workload (allocate a big data set, drop it,
 * repeat) cannot produce a mixed pause no matter how it is tuned:
 *
 * <ul>
 *   <li>Data allocated together is PROMOTED together, so a set that is dropped
 *       all at once leaves regions that are <b>100% dead</b> — and cleanup
 *       frees those in place, at the end of the mark cycle, with no mixed pause
 *       involved. The reclamation happens and the mixed path still never runs.
 *   <li>A monotonically growing retained set leaves regions that are
 *       <b>~100% live</b>, and `mixed_gc_live_threshold_percent` (85) refuses
 *       those by construction: copying a 98%-live region to reclaim 2% is not
 *       worth a pause.
 * </ul>
 *
 * A mixed candidate is a region between those two, and the only way to build
 * one is to make objects with DIFFERENT lifetimes get promoted in the SAME
 * pause. So the probe allocates its two long-lived classes strictly
 * round-robin, one node each, from one thread and one TLAB:
 *
 * <ul>
 *   <li><b>long</b> — enters a {@code ring} slot and lives {@code ringPhases}
 *       phases;
 *   <li><b>short</b> — lives exactly one phase, then is dropped.
 * </ul>
 *
 * Both are kept reachable through the whole of the phase that creates them, so
 * both age past the tenuring threshold and are promoted together, into the same
 * Old regions, interleaved node by node. When the short class is dropped at the
 * start of the next phase, every Old region that phase produced goes to about
 * <b>half live</b> — under the 85% threshold, over the 0% that cleanup handles
 * for free, and holding half a region of reclaimable garbage that only a mixed
 * collection can get back.
 *
 * <h2>3. The live set must be BOUNDED</h2>
 *
 * The one configuration that reached a mixed pause in wave 5 did so by running
 * out of heap, and that pause copied nothing because there was nothing left to
 * copy into. A probe whose retained set grows monotonically will always end
 * that way; it is a race between the mark cycle and the allocator that the
 * allocator wins. So the {@code ring} is a fixed-size array: phase {@code p}
 * drops slot {@code p % ringPhases} before it fills it, and the retained set
 * sits at {@code (ringPhases + 1) × perPhaseMiB / 2} for the whole run, no
 * matter how many phases are asked for. Steady state is the point: a run of 40
 * phases and a run of 10 phases put the same pressure on the collector, so the
 * phase count is a sample-size knob and not a heap-size knob.
 *
 * <h2>THE CHECKSUM</h2>
 *
 * Every node folds its tag and payload length in at creation; every walk folds
 * the tags it mutates; and a final pass folds every survivor, ring slot by ring
 * slot in index order and then along each chain. Deterministic, independent of
 * the collector, and independent of pause timing — no identity hashes, no
 * clocks, no iteration over anything a GC could reorder. A run that is faster
 * because it lost an object, or because a reference into a moved object was not
 * fixed up, produces a different number and FAILS rather than scores.
 *
 * <h2>USAGE</h2>
 *
 * <pre>
 * G1MixedRepeatProbe &lt;perPhaseMiB&gt; &lt;ringPhases&gt; &lt;phases&gt; &lt;youngChurnMiB&gt; [walkNodes] [forceCycleEvery]
 * </pre>
 *
 * Defaults {@code 12 6 24 24 4096 0}. Retained set at the defaults is
 * {@code (6 + 1) × 6 = 42 MiB}, which wants a heap of roughly three times that
 * so the collector has room to work; the recipe in
 * {@code docs/internal/g1-2026-09-20/w6m-*.md} pins the measured one.
 */
public final class G1MixedRepeatProbe {

    /** One node: header, ref, tag, payload. 256 bytes all in. */
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

    /** Nominal cost of one node including both headers. */
    static final int NODE_BYTES = 256;

    /** Payload bytes per node, leaving room for the headers and the fields. */
    static final int PAYLOAD_BYTES = NODE_BYTES - 32;

    public static void main(String[] args) {
        int perPhaseMiB = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        int ringPhases = args.length > 1 ? Integer.parseInt(args[1]) : 6;
        int phases = args.length > 2 ? Integer.parseInt(args[2]) : 24;
        int youngChurnMiB = args.length > 3 ? Integer.parseInt(args[3]) : 24;
        int walkNodes = args.length > 4 ? Integer.parseInt(args[4]) : 4096;
        int forceCycleEvery = args.length > 5 ? Integer.parseInt(args[5]) : 0;
        if (ringPhases < 1) {
            ringPhases = 1;
        }
        if (walkNodes < 0) {
            walkNodes = 0;
        }

        // Half the phase's long-lived bytes go to the ring, half to the
        // one-phase class. The 50/50 split is what puts a promoted region at
        // ~50% live after the drop, which is the middle of the band between
        // "cleanup frees it for nothing" and "the 85% threshold refuses it".
        final int halfNodes = (perPhaseMiB * 1024 * 1024) / (2 * NODE_BYTES);
        final int youngChurnNodes = (youngChurnMiB * 1024 * 1024) / NODE_BYTES;

        long checksum = 0;
        long nodesAllocated = 0;
        long longBytesDropped = 0;
        long shortBytesDropped = 0;

        final Node[] ring = new Node[ringPhases];
        Node shortPrev = null;

        long start = System.nanoTime();

        for (int phase = 0; phase < phases; phase++) {
            final int slot = phase % ringPhases;

            // ---- the drop ---------------------------------------------------
            //
            // Both classes die HERE, at the top of the phase, after a full
            // phase of young churn has promoted them. Two stores, and between
            // them about `perPhaseMiB` of Old-generation data becomes garbage
            // that is INTERLEAVED with live data at node granularity — which is
            // the whole point of the probe and the only shape a mixed pause can
            // act on.
            if (ring[slot] != null) {
                longBytesDropped += (long) halfNodes * NODE_BYTES;
                ring[slot] = null;
            }
            if (shortPrev != null) {
                shortBytesDropped += (long) halfNodes * NODE_BYTES;
                shortPrev = null;
            }

            // ---- allocate, strictly round-robin -----------------------------
            //
            // One long node, one short node, one after the other, from this
            // thread's TLAB. They land adjacent in Eden, survive the same young
            // pauses, and are copied by the same evacuation into the same Old
            // regions. Anything that batches the two classes — a stripe, a
            // separate loop, a second thread — segregates them by region again
            // and the probe stops working.
            Node longChain = null;
            Node shortChain = null;
            int tag = phase * 1_000_003;
            for (int i = 0; i < halfNodes; i++) {
                longChain = new Node(longChain, PAYLOAD_BYTES, tag + i);
                checksum += longChain.tag + longChain.payload.length;
                shortChain = new Node(shortChain, PAYLOAD_BYTES, tag - i);
                checksum += shortChain.tag + shortChain.payload.length;
                nodesAllocated += 2;
            }
            ring[slot] = longChain;
            shortPrev = shortChain;

            // ---- age, and drive the pauses ----------------------------------
            //
            // Pure young garbage, retained by nothing. Two jobs: it is what
            // makes young pauses happen at all, and it is what ages the two
            // chains above past the tenuring threshold so they are PROMOTED
            // rather than merely surviving in Survivor space. A phase with too
            // little of this leaves both classes young when the next drop
            // comes, the old generation never acquires any garbage, and every
            // Old region reads as ~100% live.
            for (int i = 0; i < youngChurnNodes; i++) {
                Node dead = new Node(null, PAYLOAD_BYTES, i);
                checksum += dead.payload.length;
                nodesAllocated++;
                // Touch the retained set from inside the churn, so the
                // reference-store barrier runs with an OLD receiver and the
                // remembered sets that a mixed pause has to scan are real
                // rather than empty. Every 64th iteration keeps this off the
                // allocation fast path's critical loop.
                if ((i & 63) == 0) {
                    checksum += walk(ring[slot], 64, phase);
                }
            }

            // A bounded walk of every ring slot, so the whole retained set is
            // provably reachable across the phase boundary and the marker has
            // to follow real edges to find it.
            for (int s = 0; s < ringPhases; s++) {
                checksum += walk(ring[s], walkNodes, phase);
            }
            checksum += walk(shortPrev, walkNodes, phase);

            // OPT-IN, default off. The flag-free route to a completed mark
            // cycle, for a host on which neither `--nojit` nor
            // `CRATONVM_G1_JIT_MARK_DRIVER=1` is wanted. It changes what is
            // being measured — a forced cycle is not an IHOP-triggered one —
            // so any arm that sets it must say so.
            if (forceCycleEvery > 0 && (phase + 1) % forceCycleEvery == 0) {
                System.gc();
            }
        }

        long wallMs = (System.nanoTime() - start) / 1_000_000L;

        // Final fold over everything that survived, in a fixed order.
        long liveNodes = 0;
        long liveFold = 0;
        for (int s = 0; s < ringPhases; s++) {
            for (Node cur = ring[s]; cur != null; cur = cur.next) {
                liveFold += cur.tag + cur.payload.length;
                liveNodes++;
            }
        }
        for (Node cur = shortPrev; cur != null; cur = cur.next) {
            liveFold += cur.tag + cur.payload.length;
            liveNodes++;
        }
        checksum += liveFold + liveNodes;

        System.out.println("G1MixedRepeatProbe"
                + " perPhase=" + perPhaseMiB + "MiB"
                + " ringPhases=" + ringPhases
                + " phases=" + phases
                + " youngChurn=" + youngChurnMiB + "MiB"
                + " walkNodes=" + walkNodes
                + " forceCycleEvery=" + forceCycleEvery
                + " liveNodes=" + liveNodes
                + " liveMiB=" + ((liveNodes * NODE_BYTES) / (1024 * 1024))
                + " nodesAllocated=" + nodesAllocated
                + " longDroppedMiB=" + (longBytesDropped / (1024 * 1024))
                + " shortDroppedMiB=" + (shortBytesDropped / (1024 * 1024))
                + " wallMs=" + wallMs
                + " checksum=" + checksum);
    }

    /**
     * Walk up to {@code limit} nodes, mutating each tag so the write barrier
     * runs on a receiver that is already old, and folding the result into a
     * return value the caller adds to the checksum. Bounded, so the walk cost
     * does not grow with the retained set and turn this into a pointer-chasing
     * benchmark; deterministic, so the checksum is.
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
