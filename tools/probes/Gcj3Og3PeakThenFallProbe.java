// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.util.ArrayList;

/**
 * gcj j3/og3 (2026-10-19): a live set that PEAKS and then FALLS, followed by a
 * young-only churn phase -- the deciding shape of blocker B2 of
 * {@code gcj-j2og2-free-ratio-growth-flip-blockers} (the old capacity never
 * came back down, so the young trigger stayed capped at the peak's
 * {@code (Xmx - old capacity) / 2} for the rest of the run) and of the
 * Serial-shaped major trigger.
 *
 * <p>Phase 1 grows a live set of {@code peakPct} % of {@code Runtime.maxMemory()}
 * in 1 MiB arrays (it tenures; on Generational with
 * {@code CRATONVM_GC_OLD_FREE_RATIO_GROWTH=1} the old capacity grows into the
 * young budget). Phase 2 drops it and runs {@code System.gc()} twice (the
 * committed resize and, with {@code CRATONVM_GC_OLD_CAPACITY_SHRINK=1}, the
 * capacity shrink). Phase 3 allocates {@code churnMb} MiB of short-lived
 * arrays: only young collections are due, and their count is the number that
 * moves (HotSpot Serial's eden keeps its size whatever the tenured generation
 * did).
 *
 * <p>stdout is identical on HotSpot ({@code -XX:+UseSerialGC}) and CratonVM:
 * <pre>
 *   Gcj3Og3PeakThenFallProbe peakPct=60 churnMb=2048
 *   peak-arrays=ok churn-sum=&lt;churnMb * 64&gt;
 *   PROBE-OK
 * </pre>
 * (deterministic); stderr (informational):
 * {@code [og3-ptf] young-gcs-churn=<n> full-gcs=<n> tenured-max-mb-before=<n>
 * tenured-max-mb-after=<n> total-mb-peak=<n> total-mb-end=<n> churn-ms=<n>}.
 *
 * <p>Args (optional): {@code peakPct churnMb}.
 */
public final class Gcj3Og3PeakThenFallProbe {
    static volatile Object sink;

    public static void main(String[] args) {
        int peakPct = args.length > 0 ? Integer.parseInt(args[0]) : 60;
        int churnMb = args.length > 1 ? Integer.parseInt(args[1]) : 2048;
        System.out.println("Gcj3Og3PeakThenFallProbe peakPct=" + peakPct + " churnMb=" + churnMb);
        Runtime rt = Runtime.getRuntime();
        long tenuredMaxBefore = tenuredMax();
        int peak = (int) (rt.maxMemory() / (1 << 20) * peakPct / 100);
        ArrayList<long[]> live = new ArrayList<>(peak);
        for (int i = 0; i < peak; i++) {
            live.add(new long[(1 << 20) / 8 - 4]);
        }
        long totalPeak = rt.totalMemory();
        int peakArrays = live.size();
        live = null;
        System.gc();
        System.gc();
        long tenuredMaxAfter = tenuredMax();
        long young0 = youngCount();
        long t0 = System.nanoTime();
        long sum = churn(churnMb);
        long churnMs = (System.nanoTime() - t0) / 1_000_000;
        long youngGcs = youngCount() - young0;
        System.out.println("peak-arrays=" + (peakArrays > 0 ? "ok" : "none") + " churn-sum=" + sum);
        System.out.println("PROBE-OK");
        System.err.println("[og3-ptf] young-gcs-churn=" + youngGcs + " full-gcs=" + fullCount()
                + " tenured-max-mb-before=" + (tenuredMaxBefore >> 20)
                + " tenured-max-mb-after=" + (tenuredMaxAfter >> 20)
                + " total-mb-peak=" + (totalPeak >> 20) + " total-mb-end=" + (rt.totalMemory() >> 20)
                + " churn-ms=" + churnMs + " peak-arrays=" + peakArrays);
    }

    /** {@code mb} MiB of 8 KiB arrays, each dead at once (gcj j3/og3, j3a: 64 KiB ones
     * showed 3-4 young collections per 2 GiB against HotSpot's 30, so they did not
     * churn the young generation). */
    static long churn(int mb) {
        long acc = 0;
        for (int i = 0; i < mb * 128; i++) {
            long[] a = new long[(1 << 13) / 8 - 4];
            a[i & 7] = i;
            acc += a[i & 7] & 1;
        }
        return acc;
    }

    static long tenuredMax() {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            String n = p.getName();
            if (n.contains("Tenured") || n.contains("Old")) {
                return Math.max(0, p.getUsage().getMax());
            }
        }
        return -1;
    }

    static long youngCount() {
        long c = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            String n = b.getName();
            if (n.contains("Copy") || n.contains("Young") || n.contains("Scavenge")) {
                c += Math.max(0, b.getCollectionCount());
            }
        }
        return c;
    }

    static long fullCount() {
        long c = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            String n = b.getName();
            if (n.contains("MarkSweep") || n.contains("Old") || n.contains("Full")) {
                c += Math.max(0, b.getCollectionCount());
            }
        }
        return c;
    }
}
