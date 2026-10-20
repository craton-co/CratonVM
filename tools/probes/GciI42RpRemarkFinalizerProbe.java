// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;

/**
 * gci i42/rp: does a finalizable object that only a G1 concurrent cycle's remark
 * finds dead get finalized promptly, without waiting for the next young pause?
 *
 * <p>Page: {@code docs/known-issues/gc/gci-i42rp-remark-queued-finalizers-wait-for-the-next-collection-20261016.md}.
 * A {@code Fin} is promoted to the old generation by two {@code System.gc()} calls
 * and then dropped, so no young pause can judge it; a JIT-hot churn loop then
 * retains enough data to start concurrent cycles. After every cycle the main
 * thread notices (the {@code G1 Concurrent GC} bean's count moved), it polls for
 * up to 400 ms with tiny allocations and sleeps -- far too little allocation for a
 * young pause at {@code -Xmx128m}. On HotSpot the Finalizer thread runs
 * {@code finalize()} as soon as the remark has discovered the object.
 *
 * <p>Usage: {@code GciI42RpRemarkFinalizerProbe} (no arguments; G1 only, any other
 * collector prints {@code not-g1}). Stdout is deterministic
 * ({@code finalized true}); the facts that decide the page go to stderr:
 * {@code [i42rp] finalized=<b> in_poll=<b> young_during_poll=<n> cycles=<n> sink=<n>
 * engaged=<b> pre_drop=<n>} --
 * {@code in_poll} is whether {@code finalize()} ran while the main thread was in a
 * poll window, {@code young_during_poll} the young collections that completed in
 * that window before it ran (-1 when it did not run in one), {@code cycles} the
 * concurrent-bean moves after the drop.
 *
 * <p>gci i47w/j42 revision: the {@code Fin} stays strongly reachable (in
 * {@code keep}) while the churn runs until the {@code G1 Concurrent GC} bean's
 * count has moved once ({@code pre_drop} = the moves seen before the drop), so by
 * the time it dies the old generation is past the concurrent-start threshold and
 * the next marking cycle's remark -- not a young pause -- is what finds it dead.
 * {@code engaged=true} iff at least one move happened while the {@code Fin} was
 * reachable (within {@link #PRE_DROP_BUDGET_NS}); a row with {@code engaged=false}
 * never reached the remark door and decides nothing. Stdout is unchanged.
 */
public class GciI42RpRemarkFinalizerProbe {
    static volatile boolean finalized;
    static volatile boolean inPoll;
    static volatile boolean ranInPoll;
    static volatile long youngAtFinalize = -1;
    static GarbageCollectorMXBean young;
    static GarbageCollectorMXBean conc;
    static Object keep;
    static final ArrayList<int[]> RETAINED = new ArrayList<>();
    static int sink;
    /** Phase 1 budget: the churn that must move the concurrent bean while the Fin is reachable. */
    static final long PRE_DROP_BUDGET_NS = 20_000_000_000L;

    static final class Fin {
        final int[] pad = new int[16];

        @Override
        @SuppressWarnings("deprecation")
        protected void finalize() {
            youngAtFinalize = young.getCollectionCount();
            ranInPoll = inPoll;
            finalized = true;
        }
    }

    /** JIT-hot: retained 1 KiB chunks plus short-lived garbage. */
    static void churn() {
        for (int i = 0; i < 2000; i++) {
            RETAINED.add(new int[256]);
            int[] g = new int[64];
            sink += g.length;
        }
        if (RETAINED.size() > 70_000) {
            RETAINED.subList(0, 35_000).clear();
        }
    }

    /** At most 400 ms of almost no allocation; true once finalize() has run. */
    static long poll() throws InterruptedException {
        long y0 = young.getCollectionCount();
        inPoll = true;
        try {
            for (int i = 0; i < 200 && !finalized; i++) {
                Object[] o = new Object[1];
                sink += o.length;
                Thread.sleep(2);
            }
        } finally {
            inPoll = false;
        }
        return y0;
    }

    public static void main(String[] args) throws Exception {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().equals("G1 Young Generation")) {
                young = b;
            } else if (b.getName().equals("G1 Concurrent GC")) {
                conc = b;
            }
        }
        if (young == null || conc == null) {
            System.out.println("not-g1");
            return;
        }
        keep = new Fin();
        System.gc();
        System.gc();
        // Phase 1: keep the Fin reachable until a concurrent cycle has moved the bean.
        long concStart = conc.getCollectionCount();
        long preDeadline = System.nanoTime() + PRE_DROP_BUDGET_NS;
        while (conc.getCollectionCount() == concStart && System.nanoTime() < preDeadline) {
            churn();
        }
        long preDrop = conc.getCollectionCount() - concStart;
        boolean engaged = preDrop > 0;
        keep = null;
        // Phase 2: as before -- churn, and after every bean move poll with almost no allocation.
        long conc0 = conc.getCollectionCount();
        long lastConc = conc0;
        long youngAtPoll = -1;
        long deadline = System.nanoTime() + 30_000_000_000L;
        while (!finalized && System.nanoTime() < deadline) {
            churn();
            long c = conc.getCollectionCount();
            if (c != lastConc) {
                lastConc = c;
                youngAtPoll = poll();
            }
        }
        RETAINED.clear();
        boolean done = finalized;
        long youngDuringPoll = (done && ranInPoll) ? youngAtFinalize - youngAtPoll : -1;
        System.out.println("finalized " + done);
        System.err.println("[i42rp] finalized=" + done + " in_poll=" + (done && ranInPoll)
                + " young_during_poll=" + youngDuringPoll + " cycles=" + (lastConc - conc0)
                + " sink=" + (sink & 1) + " engaged=" + engaged + " pre_drop=" + preDrop);
    }
}
