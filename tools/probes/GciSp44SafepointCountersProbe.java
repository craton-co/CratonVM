// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Method;

/**
 * gci i44/sp ({@code gci-i44sp-safepoint-counters-answer-zero-20261016}): the
 * safepoint counters of {@code sun.management.HotspotRuntimeMBean} against
 * HotSpot 25. Needs {@code --add-exports java.management/sun.management=ALL-UNNAMED}
 * on both sides.
 *
 * <p>Reads the count, the total stopped time and the sync time, runs
 * {@code args[0]} (default 5) {@code System.gc()} calls with a short sleep
 * between them, reads them again. Only predicates are printed, so the stdout
 * is deterministic on both VMs.
 *
 * <p>stdout:
 * <pre>
 * sp44 count_grew=true        (at least one safepoint per System.gc())
 * sp44 nonneg=true            (no counter negative before the calls)
 * sp44 monotone=true          (count and both times never go backwards)
 * sp44 sync_le_total=true     (sync time within total stopped time)
 * </pre>
 * stderr (diagnostic): the raw before / after figures.
 */
public class GciSp44SafepointCountersProbe {
    public static void main(String[] args) throws Exception {
        int gcs = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        Object rt = Class.forName("sun.management.ManagementFactoryHelper")
                .getMethod("getHotspotRuntimeMBean")
                .invoke(null);
        Class<?> itf = Class.forName("sun.management.HotspotRuntimeMBean");
        Method count = itf.getMethod("getSafepointCount");
        Method total = itf.getMethod("getTotalSafepointTime");
        Method sync = itf.getMethod("getSafepointSyncTime");

        long c0 = (Long) count.invoke(rt);
        long t0 = (Long) total.invoke(rt);
        long s0 = (Long) sync.invoke(rt);
        for (int i = 0; i < gcs; i++) {
            System.gc();
            Thread.sleep(2);
        }
        long c1 = (Long) count.invoke(rt);
        long t1 = (Long) total.invoke(rt);
        long s1 = (Long) sync.invoke(rt);

        System.err.println("sp44 raw count " + c0 + " -> " + c1 + ", total_ms " + t0 + " -> " + t1
                + ", sync_ms " + s0 + " -> " + s1);
        System.out.println("sp44 count_grew=" + (c1 - c0 >= gcs));
        System.out.println("sp44 nonneg=" + (c0 >= 0 && t0 >= 0 && s0 >= 0));
        System.out.println("sp44 monotone=" + (c1 >= c0 && t1 >= t0 && s1 >= s0));
        System.out.println("sp44 sync_le_total=" + (s1 <= t1));
    }
}
