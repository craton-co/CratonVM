// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.List;

/**
 * gce e1/o (2026-10-04): H2's {@code Utils.collectGarbage()} loop ends on G1
 * and ZGC once {@code getCollectionTime()} is HotSpot's floor of the summed
 * pauses -- it no longer moves on every collection.
 *
 * <p>Page:
 * {@code docs/internal/gc/gce-e1o-g1-zgc-collection-time-adds-a-millisecond-per-pause-FIXED-20261004.md}.
 * H2 reads {@code count = sum(getCollectionTime()) / beans}, rounded, and
 * spins {@code while (count == that) System.gc();}. The probe runs that loop
 * {@value #LOOPS} times, each bounded at {@value #MAX_GCS} {@code System.gc()}
 * calls, and checks that every loop ended within its bound.
 *
 * <p>Deterministic stdout (HotSpot 25 with {@code -XX:+UseG1GC},
 * {@code -XX:+UseZGC}, {@code -XX:+UseSerialGC}):
 * <pre>
 *   h2-collect-garbage-loops-ended=true
 * </pre>
 * Stderr (not compared): {@code [probe] loops=.. gcs=.. max_gcs_in_one_loop=..
 * time_ms=.. count=..}.
 * <pre>
 *   cratonvm --java-home "$JDK" -XX:+UseZGC -Xmx256m -cp tools/bench Gce1oH2CollectGarbageProbe
 * </pre>
 */
public final class Gce1oH2CollectGarbageProbe {
    static final int LOOPS = 5;
    static final int MAX_GCS = 2_000;
    static volatile Object sink;

    static long h2Count(List<GarbageCollectorMXBean> beans) {
        long sum = 0;
        for (GarbageCollectorMXBean gc : beans) {
            sum += Math.max(0, gc.getCollectionTime());
        }
        return Math.round((double) sum / beans.size());
    }

    public static void main(String[] args) {
        List<GarbageCollectorMXBean> beans = ManagementFactory.getGarbageCollectorMXBeans();
        boolean ended = true;
        int gcs = 0;
        int worst = 0;
        for (int l = 0; l < LOOPS; l++) {
            long count = h2Count(beans);
            int n = 0;
            while (count == h2Count(beans) && n < MAX_GCS) {
                for (int k = 0; k < 4096; k++) {
                    sink = new long[64];
                }
                System.gc();
                n++;
            }
            ended &= n < MAX_GCS;
            gcs += n;
            worst = Math.max(worst, n);
        }
        long time = 0;
        long count = 0;
        for (GarbageCollectorMXBean gc : beans) {
            time += gc.getCollectionTime();
            count += gc.getCollectionCount();
        }
        System.out.println("h2-collect-garbage-loops-ended=" + ended);
        System.err.println("[probe] loops=" + LOOPS + " gcs=" + gcs + " max_gcs_in_one_loop=" + worst
                + " time_ms=" + time + " count=" + count);
    }
}
