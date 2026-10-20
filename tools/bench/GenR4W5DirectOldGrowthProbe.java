// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w5/concmark5 (2026-09-24): old-generation growth with NO young
 * collections — the workload
 * {@code docs/internal/gaps/gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924.md}
 * is about.
 *
 * <p>The program allocates nothing but humongous {@code long[]} arrays (each
 * larger than half a young semi-space at {@code -Xmx512m}, so the generational
 * heap places them straight into the old generation), keeps the last
 * {@code keep} of them in a ring and drops the rest. Every dropped array is old
 * garbage that only an old-generation collection can reclaim, and no young
 * collection ever runs to consider starting one. Between two allocations the
 * program walks the ring (a strided read-modify-write), which gives a
 * concurrent cycle time to run.
 *
 * <p>What the generational heap should do (read from the
 * {@code CRATONVM_DBG=gc-stats} shutdown summary; HotSpot has no equivalent
 * counters):
 * <ul>
 *   <li><b>default (no service thread)</b>: nothing considers the concurrent
 *       start until the old generation is full and an allocation fails over to
 *       a young collection: {@code [GC] conc_driver: ... concdrv_growth_signals=0},
 *       and the old generation is reclaimed mostly by stop-the-world
 *       collections ({@code [GC] generational: minor=... major=M}, {@code M >= 1}).</li>
 *   <li><b>{@code CRATONVM_GEN_CONC_SERVICE_THREAD=1}</b> (needs the VM-side
 *       service thread, cross-lane request in
 *       {@code docs/internal/reviews/gengc-round4-w5-concmark5-20260924.md}):
 *       the second allocation crosses the 45 % start threshold, the direct
 *       allocation poll wakes the service, and the service's concurrent cycle
 *       reclaims the dropped arrays: {@code concdrv_growth_signals >= 1},
 *       {@code concdrv_service_cycles_completed >= 1},
 *       {@code [GC] conc_policy: ... concpol_cycles_completed=N} with
 *       {@code N >= iters / 4}, and {@code major=M} with {@code M <= 1}.</li>
 * </ul>
 * Program output is a pure function of the arguments and must match HotSpot's
 * byte for byte:
 * <pre>
 *   direct-old-growth mib=80 keep=1 iters=48 checksum=&lt;same on every VM&gt;
 * </pre>
 * Commands (build: {@code javac -d tools/bench tools/bench/GenR4W5DirectOldGrowthProbe.java}):
 * <pre>
 *   java -XX:+UseSerialGC -Xmx512m -cp tools/bench GenR4W5DirectOldGrowthProbe
 *   java -XX:+UseG1GC -Xmx512m -Xlog:gc -cp tools/bench GenR4W5DirectOldGrowthProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m \
 *       -cp tools/bench GenR4W5DirectOldGrowthProbe
 *   CRATONVM_GEN_CONC_SERVICE_THREAD=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W5DirectOldGrowthProbe
 * </pre>
 * The G1 command shows HotSpot's shape: humongous allocations start
 * concurrent cycles ({@code Pause Young (Concurrent Start) (G1 Humongous Allocation)})
 * with no ordinary young collection needed.
 *
 * <p>Usage: {@code GenR4W5DirectOldGrowthProbe [mib] [keep] [iters]}.
 */
public final class GenR4W5DirectOldGrowthProbe {
    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    public static void main(String[] args) {
        final int mib = arg(args, 0, 80);
        final int keep = arg(args, 1, 1);
        final int iters = arg(args, 2, 48);
        final int len = mib * (1024 * 1024 / 8);
        final int stride = 64;
        final long[][] ring = new long[keep][];
        long checksum = 0;
        for (int i = 0; i < iters; i++) {
            long[] a = new long[len];
            for (int j = 0; j < len; j += stride) {
                a[j] = (long) i * 1_000_003L + j;
            }
            ring[i % keep] = a;
            // The pacing walk: read back every live array in the ring.
            for (long[] r : ring) {
                if (r == null) {
                    continue;
                }
                long s = 0;
                for (int j = 0; j < len; j += stride) {
                    s += r[j];
                    r[j] = s & 0xFFFF;
                }
                checksum = checksum * 31 + s;
            }
        }
        System.out.println("direct-old-growth mib=" + mib + " keep=" + keep + " iters=" + iters
                + " checksum=" + checksum);
    }
}
