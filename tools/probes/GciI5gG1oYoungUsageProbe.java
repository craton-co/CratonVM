// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;

/**
 * gci i5g/g1o (2026-10-12): which G1 pauses set {@code G1 Old Gen}'s
 * collection usage ({@code getCollectionUsage().getUsed()}), as HotSpot 25
 * answers it. Page:
 * {@code docs/internal/gc/gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage-RETIRED-UNFIXED-20261018.md}.
 *
 * <p>HotSpot 25 ({@code g1MonitoringSupport.cpp}): the young manager lists
 * {@code G1 Old Gen} as not always affected, so a young-only pause leaves its
 * collection usage alone (it reads {@code 0} until a pause that affects it);
 * the concurrent manager's Remark / Cleanup set it. Run HotSpot with
 * {@code -XX:+ExplicitGCInvokesConcurrent} so {@code System.gc()} is a
 * concurrent cycle, as on this VM's G1.
 *
 * <p>stdout is deterministic booleans; the figures go to stderr
 * ({@code [probe] ...}). Lines:
 * <ol>
 *   <li>{@code young pauses ran: true}</li>
 *   <li>{@code old collection usage after young-only pauses is 0: <b>}</li>
 *   <li>{@code concurrent pauses counted: true}</li>
 *   <li>{@code the concurrent cycle set the old collection usage: <b>}</li>
 *   <li>{@code young pauses ran again: true}</li>
 *   <li>{@code young-only pauses left the old collection usage alone: <b>}</li>
 *   <li>{@code DONE}</li>
 * </ol>
 */
public final class GciI5gG1oYoungUsageProbe {
    static volatile Object sink;
    /** Humongous at -Xmx64m (1 MiB regions): G1 Old Gen is never empty. */
    static byte[] keep = new byte[2 << 20];

    static GarbageCollectorMXBean bean(String name) {
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (b.getName().equals(name)) {
                return b;
            }
        }
        return null;
    }

    /** Short-lived garbage only, until the young bean moved {@code n} times (20 s cap). */
    static boolean youngPauses(GarbageCollectorMXBean young, int n) {
        long y0 = young.getCollectionCount();
        long deadline = System.nanoTime() + 20_000_000_000L;
        while (young.getCollectionCount() < y0 + n && System.nanoTime() < deadline) {
            sink = new byte[4096];
        }
        sink = null;
        return young.getCollectionCount() >= y0 + n;
    }

    public static void main(String[] args) throws Exception {
        MemoryPoolMXBean old = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals("G1 Old Gen")) {
                old = p;
            }
        }
        GarbageCollectorMXBean young = bean("G1 Young Generation");
        GarbageCollectorMXBean conc = bean("G1 Concurrent GC");
        if (old == null || young == null || conc == null) {
            System.out.println("not G1 beans");
            return;
        }
        long y0 = young.getCollectionCount();
        boolean ran = youngPauses(young, 3);
        long u1 = old.getCollectionUsage().getUsed();
        System.out.println("young pauses ran: " + ran);
        System.out.println("old collection usage after young-only pauses is 0: " + (u1 == 0));

        long k0 = conc.getCollectionCount();
        System.gc();
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (conc.getCollectionCount() < k0 + 2 && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        long k1 = conc.getCollectionCount();
        long u2 = old.getCollectionUsage().getUsed();
        System.out.println("concurrent pauses counted: " + (k1 > k0));
        System.out.println("the concurrent cycle set the old collection usage: " + (u2 > 0));

        long y2 = young.getCollectionCount();
        boolean ranAgain = youngPauses(young, 3);
        long u3 = old.getCollectionUsage().getUsed();
        System.out.println("young pauses ran again: " + ranAgain);
        System.out.println("young-only pauses left the old collection usage alone: " + (u3 == u2));
        System.err.println("[probe] u1=" + u1 + " u2=" + u2 + " u3=" + u3
                + " young=" + y0 + "->" + y2 + "->" + young.getCollectionCount()
                + " conc=" + k0 + "->" + k1 + " keep=" + keep.length);
        System.out.println("DONE");
    }
}
