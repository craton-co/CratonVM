// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zr zs s1/tp (2026-10-14): the director's storm shape, for
// CRATONVM_ZGC_DIRECTOR_REARM_FLOOR.
//
// A live set held ABOVE the high-usage point of -XX:SoftMaxHeapSize (95% of
// the soft max) but below the live-bytes threshold (75% of -Xmx), then a
// fixed amount of short-lived garbage. Under CRATONVM_ZGC_RATE_TRIGGER=1 the
// high-usage rule is true right after every collection; without the floor
// each collection is followed by another one the moment a poll site asks
// (every 4 MiB of compiled refills), with the floor it waits for the same
// `live + free / 4` progress as the occupancy clauses.
//
// stdout (deterministic, compare with HotSpot):
//   zs1tp director-storm live=<liveMiB> garbage=<garbageMiB> kept=<n> ok
// stderr (the measurement; never compared):
//   [probe] zs1tp storm cpu_ms=<main thread CPU> gc_count=<sum over beans> beans=<name=count,...>
//
// Run: cratonvm -Xmx256m -XX:SoftMaxHeapSize=128m --verbose:gc -cp tools/probes Zs1TpDirectorStorm [liveMiB=140] [garbageMiB=2048]
//      java -XX:+UseZGC -Xmx256m -XX:SoftMaxHeapSize=128m -Xlog:gc -cp tools/probes Zs1TpDirectorStorm
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

public class Zs1TpDirectorStorm {
    static Object sink;

    public static void main(String[] args) {
        int liveMiB = args.length > 0 ? Integer.parseInt(args[0]) : 140;
        int garbageMiB = args.length > 1 ? Integer.parseInt(args[1]) : 2048;
        // The live set: 32 KiB arrays, liveMiB x 32 of them.
        byte[][] keep = new byte[liveMiB * 32][];
        for (int i = 0; i < keep.length; i++) {
            keep[i] = new byte[32 * 1024];
            keep[i][i & 1023] = (byte) i;
        }
        long cpu0 = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        // Garbage: 4 KiB arrays, garbageMiB x 256 of them.
        int n = garbageMiB * 256;
        for (int i = 0; i < n; i++) {
            byte[] g = new byte[4096];
            g[i & 4095] = (byte) i;
            sink = g;
        }
        long cpuMs = (ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime() - cpu0) / 1_000_000;
        int kept = 0;
        for (int i = 0; i < keep.length; i++) {
            if (keep[i] != null && keep[i][i & 1023] == (byte) i) {
                kept++;
            }
        }
        long count = 0;
        StringBuilder beans = new StringBuilder();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            count += Math.max(0, gc.getCollectionCount());
            if (beans.length() > 0) {
                beans.append(',');
            }
            beans.append(gc.getName().replace(' ', '_')).append('=').append(gc.getCollectionCount());
        }
        System.err.println("[probe] zs1tp storm cpu_ms=" + cpuMs + " gc_count=" + count + " beans=" + beans);
        System.out.println("zs1tp director-storm live=" + liveMiB + " garbage=" + garbageMiB + " kept=" + kept + " ok");
    }
}
