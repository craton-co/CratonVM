// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr zs s1/tp (2026-10-14): the FIXED-WORK A/B for the ZGC director
// (CRATONVM_ZGC_RATE_TRIGGER=1 and its STW translations).
//
// Zr1TrRateTriggerProbe runs for a fixed WALL time, so an arm that pauses
// more does less work in the same seconds and its CPU figure is not a cost per
// unit of work (z1's +25 % was read off it). This probe pushes a fixed number
// of bytes through a fixed live ring (`liveMiB` of byte[8192]; every `new`
// replaces a ring slot, so the garbage rate is the allocation rate), in
// phases that change the rate (full speed, then a burst / pause pattern) so the
// rate estimator has something to track; the pauses are sleeps, which cost no
// CPU and are the same in every arm. Compare /usr/bin/time user+sys per arm.
//
// stdout (deterministic, compare with HotSpot):
//   zs1tp rate-cost live=<liveMiB> gib=<totalGiB> checksum=<n> ok
// stderr (the measurement; never compared):
//   [probe] zs1tp cpu_ms=<main thread CPU> gc_count=<sum over beans> gc_time_ms=<sum> beans=<name=count,...>
//
// Run: cratonvm -Xmx1g --verbose:gc -cp tools/probes Zs1TpRateCost [liveMiB=64] [totalGiB=16]
//      java -XX:+UseZGC -Xmx1g -Xlog:gc -cp tools/probes Zs1TpRateCost
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

public class Zs1TpRateCost {
    public static void main(String[] args) throws Exception {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        int totalGiB = args.length > 1 ? Integer.parseInt(args[1]) : 16;
        byte[][] ring = new byte[liveMiB * 128][];
        for (int i = 0; i < ring.length; i++) {
            ring[i] = new byte[8192];
            ring[i][0] = (byte) i;
        }
        long cpu0 = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        // 32 MiB per batch (4096 x 8 KiB); totalGiB x 32 batches.
        int batches = totalGiB * 32;
        int k = 0;
        long sink = 0;
        for (int b = 0; b < batches; b++) {
            for (int i = 0; i < 4096; i++) {
                byte[] a = new byte[8192];
                a[i & 8191] = (byte) (b + i);
                ring[k] = a;
                k = k + 1 == ring.length ? 0 : k + 1;
            }
            sink += ring[k][k & 8191];
            // Second half of the run: every fourth batch is followed by a 20 ms
            // pause, so the 1 s rate window sees a rate change.
            if (b >= batches / 2 && (b & 3) == 3) {
                Thread.sleep(20);
            }
        }
        long cpuMs = (ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime() - cpu0) / 1_000_000;
        // Deterministic checksum over what the ring holds at the end: each
        // slot's content depends only on the batch and index that wrote it.
        long checksum = 0;
        for (int i = 0; i < ring.length; i++) {
            byte[] a = ring[i];
            for (int j = 0; j < 8192; j += 1024) {
                checksum = checksum * 31 + a[j];
            }
        }
        long count = 0;
        long time = 0;
        StringBuilder beans = new StringBuilder();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            count += Math.max(0, gc.getCollectionCount());
            time += Math.max(0, gc.getCollectionTime());
            if (beans.length() > 0) {
                beans.append(',');
            }
            beans.append(gc.getName().replace(' ', '_')).append('=').append(gc.getCollectionCount());
        }
        System.err.println("[probe] zs1tp cpu_ms=" + cpuMs + " gc_count=" + count + " gc_time_ms=" + time
                + " beans=" + beans + " sink=" + sink);
        System.out.println("zs1tp rate-cost live=" + liveMiB + " gib=" + totalGiB + " checksum=" + checksum + " ok");
    }
}
