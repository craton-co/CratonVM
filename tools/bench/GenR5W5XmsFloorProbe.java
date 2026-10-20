// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

/**
 * gen r5w5/sizer9 (2026-09-27): {@code Runtime.totalMemory()} stays at or above
 * about {@code -Xms} AFTER young collections, as on HotSpot Serial (which never
 * shrinks the heap below its initial size), rather than falling once the
 * Generational backend's young uncommit gives the evacuated semi-space back
 * ({@code docs/internal/gc/gengc-r5w3-oldgen7-young-uncommit-ignores-the-xms-floor-FIXED-20260928.md}).
 * The companion of {@code GenR5W3XmsTotalProbe}, which reads the figure before
 * the first collection.
 *
 * <p>Churns {@code churnMib} MiB of short-lived {@code long[]}s (default 1024,
 * several times the young generation at {@code -Xmx512m}), checks that at least
 * one collection ran, and prints whether {@code totalMemory()} is at least
 * seven eighths of the {@code -Xms} given as the first argument (MiB).
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xms64m -Xmx512m -cp tools/bench GenR5W5XmsFloorProbe 64
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xms64m -Xmx512m -cp tools/bench GenR5W5XmsFloorProbe 64
 * </pre>
 * HotSpot prints, and CratonVM must print:
 * <pre>
 *   collected=true
 *   total_at_least_7_8_of_xms=true
 *   PASS
 * </pre>
 * Before the fix CratonVM printed {@code total_at_least_7_8_of_xms=false} and
 * {@code FAIL}. Add {@code -v} as the third argument to print the figures.
 */
public final class GenR5W5XmsFloorProbe {
    static volatile long[] sink;

    public static void main(String[] args) {
        long xmsMib = args.length > 0 ? Long.parseLong(args[0]) : 64;
        long churnMib = args.length > 1 ? Long.parseLong(args[1]) : 1024;
        boolean verbose = args.length > 2 && args[2].equals("-v");
        long before = collections();
        // 8 KiB arrays: well below any humongous threshold, so every one is
        // young garbage.
        long arrays = (churnMib << 20) / 8192;
        for (long i = 0; i < arrays; i++) {
            long[] a = new long[1022];
            a[(int) (i % 1022)] = i;
            sink = a;
        }
        sink = null;
        boolean collected = collections() > before;
        long total = Runtime.getRuntime().totalMemory();
        long xms = xmsMib << 20;
        if (verbose) {
            System.out.println("info (not deterministic): totalMemory=" + total + " xms=" + xms
                    + " collections=" + (collections() - before));
        }
        boolean ok = total >= xms / 8 * 7;
        System.out.println("collected=" + collected);
        System.out.println("total_at_least_7_8_of_xms=" + ok);
        boolean pass = ok && collected;
        System.out.println(pass ? "PASS" : "FAIL");
        if (!pass) {
            System.exit(1);
        }
    }

    static long collections() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }
}
