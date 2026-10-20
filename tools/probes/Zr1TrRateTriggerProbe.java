// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;

/**
 * zr z1/tr (2026-10-12): the A/B for {@code CRATONVM_ZGC_RATE_TRIGGER=1}
 * (HotSpot ZGC's allocation-rate / proactive / high-usage rules decide; the
 * 75%-of-{@code -Xmx} live-bytes clause stands down) against the default
 * legacy clauses, and the shadow reading of the same rules
 * ({@code CRATONVM_ZGC_TRIGGER_SHADOW=1}).
 *
 * <p>Shape: a {@code liveMiB} live set (a ring of {@code byte[8192]}) that is
 * churned -- every {@code new} replaces a ring slot, so the live set is fixed
 * and the garbage rate is the allocation rate -- for {@code seconds} of
 * wall time, in phases that change the rate (full speed, then a
 * 10x-throttled tail) so the rate estimator has something to track.
 *
 * <p>stdout (deterministic): {@code done}. stderr:
 * {@code [probe] cpu_ms=<thread CPU> gc_count=<sum over beans>
 * gc_time_ms=<sum> bytes=<allocated>}. Read with {@code --verbose:gc}:
 * {@code [GC] zgc-trigger-shadow: ... fired=<clauses> used_at_start=<b>
 * hs_first_yes=<rule>@<b>} per cycle, and {@code [GC] zgc-trigger:}'s tallies
 * at exit.
 */
public final class Zr1TrRateTriggerProbe {
    public static void main(String[] args) throws Exception {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 10;
        ThreadMXBean tmx = ManagementFactory.getThreadMXBean();
        byte[][] ring = new byte[liveMiB * 128][];
        for (int i = 0; i < ring.length; i++) {
            ring[i] = new byte[8192];
        }
        long cpu0 = tmx.getCurrentThreadCpuTime();
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        long tailStart = System.nanoTime() + seconds * 700_000_000L;
        long bytes = 0;
        long sink = 0;
        int k = 0;
        while (System.nanoTime() < end) {
            boolean tail = System.nanoTime() > tailStart;
            for (int i = 0; i < 4096; i++) {
                byte[] b = new byte[8192];
                b[i & 8191] = (byte) i;
                ring[k] = b;
                k = (k + 1) % ring.length;
                bytes += 8192;
            }
            sink += ring[k][k & 8191];
            if (tail) {
                Thread.sleep(3); // ~10x lower rate in the last 30%
            }
        }
        long cpuMs = (tmx.getCurrentThreadCpuTime() - cpu0) / 1_000_000;
        long count = 0;
        long time = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            count += Math.max(0, gc.getCollectionCount());
            time += Math.max(0, gc.getCollectionTime());
        }
        System.err.println("[probe] cpu_ms=" + cpuMs + " gc_count=" + count + " gc_time_ms=" + time
                + " bytes=" + bytes + " sink=" + sink);
        System.out.println("done");
    }
}
