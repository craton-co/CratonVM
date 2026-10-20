// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr zs s1/tp (2026-10-14): where the three warm-up collections happen, for
// CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS.
//
// A tiny live set and a paced stream of 64 KiB garbage arrays (128 MiB/s).
// Before each chunk the probe reads heap used (Runtime total - free); when a
// collector bean's count moves, the highest reading since the previous move is
// where that collection was asked for. HotSpot's rule_warmup collects at
// 10 / 20 / 30% of the soft max (laptop, 25.0.3, -Xmx512m: 11.3,21.0,30.4);
// before zs this VM's second warm-up waited for its `live + free / 4` re-arm
// floor (~26% on this shape).
//
// stdout (deterministic, compare with HotSpot):
//   zs1tp warmup-steps sysgc=<bool> collections>=3 ok
// stderr (the measurement; never compared): the occupancy, in percent of
// Runtime.maxMemory(), before each of the first three collections:
//   [probe] zs1tp warmup used_pct=<p1>,<p2>,<p3> beans=<name=count,...>
// HotSpot's generational ZGC may run minor collections in between (their
// bean is "ZGC Minor Cycles"); only beans whose name contains "Major" are
// counted when one exists, so the twin reads the major warm-up cycles.
//
// Run: cratonvm -Xmx512m -cp tools/probes Zs1TpWarmupSteps [sysgc]
//      java -XX:+UseZGC -Xmx512m -Xlog:gc -cp tools/probes Zs1TpWarmupSteps
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.ArrayList;
import java.util.List;

public class Zs1TpWarmupSteps {
    static Object sink;

    static List<GarbageCollectorMXBean> cycleBeans() {
        List<GarbageCollectorMXBean> all = ManagementFactory.getGarbageCollectorMXBeans();
        List<GarbageCollectorMXBean> major = new ArrayList<>();
        for (GarbageCollectorMXBean b : all) {
            String n = b.getName();
            if (n.contains("Major") && !n.contains("Pause")) {
                major.add(b);
            }
        }
        return major.isEmpty() ? all : major;
    }

    static long count(List<GarbageCollectorMXBean> beans) {
        long c = 0;
        for (GarbageCollectorMXBean b : beans) {
            c += Math.max(0, b.getCollectionCount());
        }
        return c;
    }

    public static void main(String[] args) throws Exception {
        Runtime rt = Runtime.getRuntime();
        long max = rt.maxMemory();
        List<GarbageCollectorMXBean> beans = cycleBeans();
        // `sysgc`: one System.gc() before the stream. HotSpot counts warm-up
        // CYCLES (a System.gc() is not one), so its steps stay 10/20/30%.
        boolean sysgc = args.length > 0 && args[0].equals("sysgc");
        if (sysgc) {
            System.gc();
        }
        long base = count(beans);
        long[] at = new long[3];
        int seen = 0;
        // The highest occupancy read since the previous collection: a reading
        // taken after a concurrent cycle finished would be the post-cycle one.
        long peak = 0;
        // At most 8x the heap of garbage: far more than three warm-up steps.
        int limit = (int) Math.min(Integer.MAX_VALUE, max / 65536 * 8);
        long t0 = System.nanoTime();
        for (int i = 0; i < limit && seen < 3; i++) {
            long c = count(beans) - base;
            while (seen < 3 && seen < c) {
                at[seen] = peak;
                seen++;
                peak = 0;
            }
            peak = Math.max(peak, rt.totalMemory() - rt.freeMemory());
            byte[] g = new byte[65536];
            g[i & 65535] = (byte) i;
            sink = g;
            // Paced by the clock at 128 MiB/s (64 KiB per 0.5 ms): a 10 Hz
            // director overshoots ~13 MiB, 2.5% of 512 MiB. Sleeping only
            // when ahead keeps the average on a coarse-sleep OS too.
            long due = t0 + (long) (i + 1) * 500_000L;
            if (System.nanoTime() < due) {
                Thread.sleep(1);
            }
        }
        StringBuilder pct = new StringBuilder();
        for (int s = 0; s < 3; s++) {
            if (s > 0) {
                pct.append(',');
            }
            if (s < seen) {
                long tenths = at[s] * 1000 / max;
                pct.append(tenths / 10).append('.').append(tenths % 10);
            } else {
                pct.append("none");
            }
        }
        StringBuilder names = new StringBuilder();
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (names.length() > 0) {
                names.append(',');
            }
            names.append(b.getName().replace(' ', '_')).append('=').append(b.getCollectionCount());
        }
        System.err.println("[probe] zs1tp warmup used_pct=" + pct + " beans=" + names);
        System.out.println("zs1tp warmup-steps sysgc=" + sysgc + " collections>=3 " + (seen >= 3 ? "ok" : "MISSING"));
    }
}
