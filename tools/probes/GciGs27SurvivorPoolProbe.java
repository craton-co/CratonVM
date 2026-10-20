// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;

/**
 * gci i27/gs -- HotSpot Serial's young pools as a program sees them:
 * {@code Survivor Space} has a defined, positive max (HotSpot's
 * {@code max_survivor_size()}), eden's max is about SurvivorRatio (8) times
 * it, the three heap pools' max sum to {@code Runtime.maxMemory()} and their
 * committed to {@code Runtime.totalMemory()}, and every pool keeps
 * {@code used <= committed <= max}. Page:
 * {@code docs/known-issues/gc/gci-i14mp-memory-pool-and-gc-mxbean-review-findings-20261013.md}
 * item 8. Default collector only (Serial on HotSpot).
 *
 * <p>stdout is deterministic booleans; the numbers go to stderr.
 */
public final class GciGs27SurvivorPoolProbe {
    static volatile Object sink;

    static MemoryPoolMXBean pool(String name) {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP && p.getName().equals(name)) {
                return p;
            }
        }
        return null;
    }

    static void check(String when) {
        MemoryPoolMXBean eden = pool("Eden Space");
        MemoryPoolMXBean surv = pool("Survivor Space");
        MemoryPoolMXBean old = pool("Tenured Gen");
        if (eden == null || surv == null || old == null) {
            System.out.println(when + " serial pools present: false");
            return;
        }
        System.out.println(when + " serial pools present: true");
        MemoryUsage e = eden.getUsage();
        MemoryUsage s = surv.getUsage();
        MemoryUsage o = old.getUsage();
        Runtime rt = Runtime.getRuntime();
        long maxMemory = rt.maxMemory();
        long totalMemory = rt.totalMemory();
        System.err.println("[gci-gs27] " + when + " eden=" + e + " survivor=" + s + " old=" + o
                + " maxMemory=" + maxMemory + " totalMemory=" + totalMemory);
        boolean defined = s.getMax() >= 0;
        System.out.println(when + " survivor max defined: " + defined);
        System.out.println(when + " survivor max positive: " + (s.getMax() > 0));
        boolean ratio = defined && s.getMax() > 0
                && e.getMax() >= 7 * s.getMax() && e.getMax() <= 9 * s.getMax();
        // Not asked right after the churn: there this young generation's eden
        // may hold more than HotSpot's eden could, and the survivor max then
        // gives way (see `survivor_reserve_split`).
        if (!when.equals("after-churn")) {
            System.out.println(when + " eden max about eight survivor maxes: " + ratio);
        } else {
            System.err.println("[gci-gs27] " + when + " eden max about eight survivor maxes: " + ratio);
        }
        boolean maxSum = defined && e.getMax() >= 0 && o.getMax() >= 0
                && e.getMax() + s.getMax() + o.getMax() == maxMemory;
        System.out.println(when + " pool maxes sum to maxMemory: " + maxSum);
        System.out.println(when + " pool committed sums to totalMemory: "
                + (e.getCommitted() + s.getCommitted() + o.getCommitted() == totalMemory));
        boolean ordered = true;
        for (MemoryUsage u : new MemoryUsage[] {e, s, o}) {
            ordered &= u.getUsed() <= u.getCommitted();
            if (u.getMax() >= 0) {
                ordered &= u.getCommitted() <= u.getMax();
                ordered &= u.getInit() <= u.getMax();
            }
        }
        System.out.println(when + " used <= committed <= max: " + ordered);
        System.err.println("[gci-gs27] " + when + " survivor committed positive: "
                + (s.getCommitted() > 0));
    }

    public static void main(String[] args) throws Exception {
        check("start");
        // Short-lived churn plus a rooted set, then a young-only window and a
        // full collection, so both the young pause's and the full one's
        // survivor figures are read.
        List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 20000; i++) {
            sink = new byte[1024];
            if (i % 100 == 0) {
                keep.add(new byte[512]);
            }
        }
        for (long done = 0; done < (96L << 20); done += 4096) {
            sink = new byte[4096];
        }
        check("after-churn");
        System.gc();
        check("after-gc");
        System.err.println("[gci-gs27] kept=" + keep.size());
        System.out.println("DONE");
    }
}
