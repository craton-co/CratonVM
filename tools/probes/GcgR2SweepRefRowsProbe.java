// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;

/**
 * gcg g2/r (2026-10-01): the cost of the generational concurrent sweep's
 * reference-row prune (gcd-d3n-proposal-sweep-reference-row-prune-at-pause-boundaries).
 *
 * <p>Holds {@code live} (default 100000) {@code WeakReference}s whose
 * referents stay strongly reachable, all promoted to the old generation. Then
 * {@code rounds} (default 40) rounds each replace a {@value #BALLAST_MIB} MiB
 * {@code long[]} (humongous, so old on CratonVM; tenured on HotSpot Serial),
 * churn {@value #CHURN_MIB} MiB of young garbage, and drop the previous
 * round's {@value #DEAD_PER_ROUND} promoted {@code WeakReference}s (and their
 * referents): old garbage the concurrent sweep frees, rows it must prune. As
 * in {@code GenR5W5RemarkRefsProbe}, the old generation sits between the
 * concurrent start and the STW floor at {@code -Xmx256m}, so the collector
 * that frees the dead references is a concurrent cycle.
 *
 * <p>Every sweep slice that frees something walks every one of the
 * {@code live} rows on the default path; with
 * {@code CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH=1} it walks them about once per
 * pause boundary. {@code CRATONVM_DBG=gc-stats} prints both on the
 * {@code [GC] conc_driver:} line: {@code concdrv_sweep_reference_row_prunes}
 * (walks) and {@code concdrv_sweep_reference_rows_dropped} (rows).
 *
 * <p>Stdout (deterministic, identical on HotSpot):
 * {@code sweep-ref-rows live=<live>/<live> dead-cleared=true PROBE-OK}.
 * {@code dead-cleared}: a dropped {@code Reference} cannot be observed, so
 * it checks weak references to a 64-entry sample of the last batch's
 * REFERENTS, which every collector must clear once they are unreachable.
 * Stderr (not compared):
 * {@code [probe] rounds-ms=<t> old_collections=<n>}.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/probes GcgR2SweepRefRowsProbe
 *   CRATONVM_DBG=gc-stats [CRATONVM_GEN_CONC_SWEEP_REFROW_BATCH=1] \
 *     cratonvm -XX:+UseGenerationalGC -Xmx256m -cp tools/probes GcgR2SweepRefRowsProbe
 * </pre>
 */
public final class GcgR2SweepRefRowsProbe {

    static final int BALLAST_MIB = 34;
    static final int CHURN_MIB = 64;
    static final int DEAD_PER_ROUND = 8192;
    static final int SAMPLE = 64;

    static volatile Object sink;
    static volatile long[] ballast;
    /** The previous round's references and referents (static: held until replaced). */
    static volatile Object[] holdRefs;
    static volatile Object[] holdReferents;

    public static void main(String[] args) throws Exception {
        int live = args.length > 0 ? Integer.parseInt(args[0]) : 100_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;

        Object[] referents = new Object[live];
        WeakReference<?>[] refs = new WeakReference<?>[live];
        for (int i = 0; i < live; i++) {
            referents[i] = new int[] {i};
            refs[i] = new WeakReference<>(referents[i]);
        }
        promote();

        // The previous round's references and referents: promoted while held,
        // dropped one round later.
        holdRefs = null;
        holdReferents = null;
        // Weak references (kept) to a sample of the LAST dropped referents.
        WeakReference<?>[] sample = new WeakReference<?>[SAMPLE];

        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            nextBatch(r == rounds - 1 ? sample : null);
            round();
        }
        // Drop the last batch too, then let collections find it.
        holdRefs = null;
        holdReferents = null;
        boolean deadCleared = false;
        for (int attempt = 0; attempt < 50 && !deadCleared; attempt++) {
            System.gc();
            deadCleared = true;
            for (WeakReference<?> s : sample) {
                if (!s.refersTo(null)) {
                    deadCleared = false;
                    break;
                }
            }
            if (!deadCleared) {
                Thread.sleep(20);
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        sink = null;
        ballast = null;

        int liveOk = 0;
        for (int i = 0; i < live; i++) {
            if (refs[i].get() == referents[i] && ((int[]) referents[i])[0] == i) {
                liveOk++;
            }
        }
        long old = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            String n = b.getName();
            if (!n.contains("Copy") && !n.contains("Young") && !n.contains("Scavenge")) {
                old += Math.max(0, b.getCollectionCount());
            }
        }
        System.err.println("[probe] rounds-ms=" + ms + " old_collections=" + old);
        System.out.println("sweep-ref-rows live=" + liveOk + "/" + live
                + " dead-cleared=" + deadCleared
                + (liveOk == live && deadCleared ? " PROBE-OK" : " PROBE-FAIL"));
        // Keep the referents reachable to the end (the check above reads them).
        sink = referents;
    }

    /**
     * Replace the held batch (the previous one becomes garbage). In its own
     * frame, so no slot of {@code main} keeps a batch alive on any VM.
     */
    static void nextBatch(WeakReference<?>[] sample) {
        WeakReference<?>[] next = new WeakReference<?>[DEAD_PER_ROUND];
        Object[] nextReferents = new Object[DEAD_PER_ROUND];
        for (int i = 0; i < DEAD_PER_ROUND; i++) {
            nextReferents[i] = new long[2];
            next[i] = new WeakReference<>(nextReferents[i]);
        }
        if (sample != null) {
            for (int i = 0; i < SAMPLE; i++) {
                sample[i] = new WeakReference<>(nextReferents[i * (DEAD_PER_ROUND / SAMPLE)]);
            }
        }
        holdRefs = next;
        holdReferents = nextReferents;
    }

    static void round() {
        ballast = new long[(BALLAST_MIB << 20) / 8];
        final int churnObjects = CHURN_MIB << 10; // long[126] = 1 KiB each
        for (int k = 0; k < churnObjects; k++) {
            sink = new long[126];
        }
    }

    static void promote() {
        for (int pass = 0; pass < 16; pass++) {
            for (int k = 0; k < 65536; k++) {
                sink = new long[64];
            }
        }
        sink = null;
    }
}
