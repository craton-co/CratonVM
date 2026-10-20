// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;

/**
 * gci i13/ef -- G1 committed heap and evacuation failure.
 *
 * Page: docs/internal/gc/gci-i13ef-g1-evacuation-failure-pinning-and-heap-sizing-review-findings-RETIRED-RULING-20261018.md
 *
 * Modes:
 *   promote [mb]  a 24 MiB live set of 1000-byte arrays (refreshed slowly, so most of
 *                 it survives every pause and is promoted) under mb MiB (default 1024)
 *                 of 240-byte garbage. Finding 1: on a lazily committed G1 heap
 *                 (-Xms below -Xmx) the first Old destination claimed by a serial
 *                 young pause on a full committed prefix committed the whole
 *                 reservation, so Runtime.totalMemory() jumped to maxMemory().
 *   evac [mb]     a ring of 36 MiB of 1000-byte arrays rewritten mb / 6 times over
 *                 (default 216, i.e. six laps): a live young set that does not fit
 *                 the to-space at -Xmx64m. Finding 2 (evidence only): HotSpot 25 G1
 *                 labels such pauses `(Evacuation Failure: Allocation)` on -Xlog:gc.
 *
 * stdout is deterministic (a checksum of the final live set); totals on stderr:
 *   [ef13] mode=<m> start_total_mb=a peak_total_mb=b end_total_mb=c max_mb=d old_used_kb=e
 */
public class GciEf13CommitProbe {
    static Object[] keep;
    static volatile Object sink;

    static long oldUsedKb() {
        long used = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            String n = p.getName();
            if (n.contains("Old Gen") || n.contains("Tenured")) {
                used += p.getUsage().getUsed();
            }
        }
        return used >> 10;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "promote";
        Runtime rt = Runtime.getRuntime();
        long max = rt.maxMemory();
        long startTotal = rt.totalMemory();
        long peak = startTotal;
        long check = 0;
        if (mode.equals("evac")) {
            int mb = args.length > 1 ? Integer.parseInt(args[1]) : 216;
            int n = 36 * 1024;
            keep = new Object[n];
            long laps = Math.max(1, mb / 36);
            long total = laps * n;
            for (long i = 0; i < total; i++) {
                byte[] b = new byte[1000];
                b[0] = (byte) i;
                keep[(int) (i % n)] = b;
                if ((i & 1023) == 0) {
                    long t = rt.totalMemory();
                    if (t > peak) {
                        peak = t;
                    }
                }
            }
            for (int i = 0; i < n; i++) {
                check += ((byte[]) keep[i])[0];
            }
        } else {
            int mb = args.length > 1 ? Integer.parseInt(args[1]) : 1024;
            int n = 24 * 1024;
            keep = new Object[n];
            for (int i = 0; i < n; i++) {
                byte[] b = new byte[1000];
                b[0] = (byte) i;
                keep[i] = b;
            }
            long iters = (long) mb * 4096L;
            for (long i = 0; i < iters; i++) {
                byte[] g = new byte[240];
                g[0] = (byte) i;
                if ((i & 1023) == 0) {
                    int k = (int) ((i >>> 10) % n);
                    byte[] b = new byte[1000];
                    b[0] = (byte) k;
                    keep[k] = b;
                    long t = rt.totalMemory();
                    if (t > peak) {
                        peak = t;
                    }
                }
                if ((i & 0xFFFF) == 0) {
                    sink = g;
                }
            }
            for (int i = 0; i < n; i++) {
                check += ((byte[]) keep[i])[0];
            }
        }
        long endTotal = rt.totalMemory();
        if (endTotal > peak) {
            peak = endTotal;
        }
        System.out.println("GciEf13CommitProbe " + mode + " ok check=" + check);
        System.err.println("[ef13] mode=" + mode
            + " start_total_mb=" + (startTotal >> 20)
            + " peak_total_mb=" + (peak >> 20)
            + " end_total_mb=" + (endTotal >> 20)
            + " max_mb=" + (max >> 20)
            + " old_used_kb=" + oldUsedKb());
    }
}
