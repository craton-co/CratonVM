// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.ArrayList;
import java.util.List;
import java.util.TreeSet;

/**
 * gcd d10/o (2026-09-28): the NAMES the GC and heap-pool MXBeans answer, per
 * collector, against HotSpot's, and whether a collection moves any count.
 * {@code docs/known-issues/gc/gcd-d10o-g1-and-zgc-gc-log-and-mxbean-shapes-differ-from-hotspot-20260928.md}.
 * Generational and G1 answer HotSpot's names today; ZGC answers JDK 21's
 * non-generational {@code [ZGC Cycles, ZGC Pauses]} / {@code [ZHeap]}.
 *
 * <p>Prints, sorted so the output is deterministic:
 * <pre>
 *   collectors=[...]
 *   heap-pools=[...]
 *   collector &lt;name&gt; pools=[...]      (one line per collector)
 *   counted=true|false                 (some count grew across System.gc())
 * </pre>
 * Allocation is a few megabytes, no timing is printed, and it ends on its own.
 *
 * <p>HotSpot 25 (Temurin 25.0.3, {@code -Xmx256m}):
 * <pre>
 *   -XX:+UseSerialGC  collectors=[Copy, MarkSweepCompact]
 *                     heap-pools=[Eden Space, Survivor Space, Tenured Gen]
 *   -XX:+UseG1GC      collectors=[G1 Concurrent GC, G1 Old Generation, G1 Young Generation]
 *                     heap-pools=[G1 Eden Space, G1 Old Gen, G1 Survivor Space]
 *   -XX:+UseZGC       collectors=[ZGC Major Cycles, ZGC Major Pauses, ZGC Minor Cycles, ZGC Minor Pauses]
 *                     heap-pools=[ZGC Old Generation, ZGC Young Generation]
 * </pre>
 * and {@code counted=true} on every collector. Run:
 * <pre>
 *   java     -XX:+UseSerialGC       -Xmx256m -cp tools/bench Gcd1MxNamesProbe
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m -cp tools/bench Gcd1MxNamesProbe
 *   cratonvm -XX:+UseG1GC           -Xmx256m -cp tools/bench Gcd1MxNamesProbe
 *   cratonvm -XX:+UseZGC            -Xmx256m -cp tools/bench Gcd1MxNamesProbe
 * </pre>
 */
public final class Gcd1MxNamesProbe {
    static volatile Object sink;

    public static void main(String[] args) {
        TreeSet<String> collectors = new TreeSet<>();
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        long before = 0;
        for (GarbageCollectorMXBean gc : gcs) {
            collectors.add(gc.getName());
            before += Math.max(0, gc.getCollectionCount());
        }
        TreeSet<String> heapPools = new TreeSet<>();
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                heapPools.add(p.getName());
            }
        }
        System.out.println("collectors=" + collectors);
        System.out.println("heap-pools=" + heapPools);
        TreeSet<String> perCollector = new TreeSet<>();
        for (GarbageCollectorMXBean gc : gcs) {
            TreeSet<String> pools = new TreeSet<>();
            for (String p : gc.getMemoryPoolNames()) {
                pools.add(p);
            }
            perCollector.add("collector " + gc.getName() + " pools=" + pools);
        }
        for (String line : perCollector) {
            System.out.println(line);
        }
        List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 4096; i++) {
            byte[] b = new byte[1024];
            sink = b;
            if ((i & 63) == 0) {
                keep.add(b);
            }
        }
        System.gc();
        long after = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            after += Math.max(0, gc.getCollectionCount());
        }
        System.out.println("counted=" + (after > before) + " (kept " + keep.size() + ")");
    }
}
