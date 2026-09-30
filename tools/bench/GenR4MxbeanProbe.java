// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;

/**
 * Generational GC round 4, lane plumbing (2026-09-23): what the GC MXBeans say.
 *
 * <p>Run:
 * <pre>
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4MxbeanProbe
 *   java     -XX:+UseSerialGC      -Xmx256m -cp tools/bench GenR4MxbeanProbe   # reference
 * </pre>
 *
 * <p>Prints every collector bean (name, count, time, pool names) and every pool
 * (name, type, usage), then churns allocation for ~40 young collections plus
 * three {@code System.gc()} and prints the beans again.
 *
 * <p>Expected on HotSpot SerialGC: two collectors, {@code Copy} and
 * {@code MarkSweepCompact}; pools {@code Eden Space}, {@code Survivor Space},
 * {@code Tenured Gen} (plus non-heap pools) with real usage; after the churn
 * {@code Copy} count grows by tens, {@code MarkSweepCompact} by at least 3, and
 * each bean's {@code getCollectionTime()} is milliseconds, of the order of the
 * summed pauses {@code -Xlog:gc} prints.
 *
 * <p>Expected on CratonVM {@code -XX:+UseGenerationalGC} after round 4 wave 2
 * (see {@code docs/internal/gc/gengc-r4-plumbing-mxbeans-are-not-per-generation-FIXED-20260923.md}):
 * HotSpot Serial's shape. Two collectors, {@code Copy} (pools
 * {@code Eden Space,Survivor Space}) and {@code MarkSweepCompact} (pools
 * {@code Eden Space,Survivor Space,Tenured Gen}); three heap pools with real
 * usage ({@code Survivor Space} used is 0 between collections: it is the empty
 * copy reserve); after the churn {@code MarkSweepCompact} grows by at least the
 * three {@code System.gc()} and the two counts sum to the {@code GC(n)} lines of
 * {@code -Xlog:gc}. Under G1 and ZGC the pre-wave-2 answer is unchanged: one
 * collector named {@code G1 Young Generation}; pools {@code Eden Space} and
 * {@code Old Gen} with usage {@code -1}.
 * What round 4 fixed and this probe checks: {@code getCollectionTime()} is no
 * longer equal to {@code getCollectionCount()} once pauses exceed a millisecond,
 * it is monotonic, and it moves on every collection. The last line prints
 * {@code TIME_IS_COUNT=true|false}; on a generational run after round 4 it must
 * be {@code false} whenever any single pause took more than 1 ms.
 */
public final class GenR4MxbeanProbe {
    static volatile Object sink;

    public static void main(String[] args) {
        dump("before");
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        long count0 = 0;
        long time0 = 0;
        for (GarbageCollectorMXBean gc : gcs) {
            count0 += gc.getCollectionCount();
            time0 += gc.getCollectionTime();
        }
        List<byte[]> keep = new ArrayList<>();
        for (int round = 0; round < 40; round++) {
            for (int i = 0; i < 20_000; i++) {
                sink = new byte[512];
            }
            keep.add(new byte[64 * 1024]);
            if (keep.size() > 200) {
                keep.clear();
            }
        }
        for (int i = 0; i < 3; i++) {
            long before = totalTime(gcs);
            System.gc();
            long after = totalTime(gcs);
            System.out.println("system.gc#" + i + " time_before=" + before + " time_after=" + after
                    + " advanced=" + (after > before));
        }
        dump("after");
        long count1 = 0;
        long time1 = 0;
        for (GarbageCollectorMXBean gc : gcs) {
            count1 += gc.getCollectionCount();
            time1 += gc.getCollectionTime();
        }
        System.out.println("delta_count=" + (count1 - count0) + " delta_time_ms=" + (time1 - time0));
        System.out.println("TIME_IS_COUNT=" + (count1 == time1));
    }

    static long totalTime(List<GarbageCollectorMXBean> gcs) {
        long t = 0;
        for (GarbageCollectorMXBean gc : gcs) {
            t += gc.getCollectionTime();
        }
        return t;
    }

    static void dump(String label) {
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            System.out.println(label + " collector name=" + gc.getName()
                    + " count=" + gc.getCollectionCount()
                    + " time_ms=" + gc.getCollectionTime()
                    + " pools=" + String.join(",", gc.getMemoryPoolNames()));
        }
        for (MemoryPoolMXBean pool : ManagementFactory.getMemoryPoolMXBeans()) {
            MemoryUsage u = pool.getUsage();
            System.out.println(label + " pool name=" + pool.getName() + " type=" + pool.getType()
                    + " used=" + (u == null ? "null" : u.getUsed())
                    + " committed=" + (u == null ? "null" : u.getCommitted())
                    + " max=" + (u == null ? "null" : u.getMax()));
        }
    }
}
