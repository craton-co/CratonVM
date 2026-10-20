// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.TreeMap;

/**
 * gci i30/ob -- what one {@code System.gc()} books on the GC beans
 * ({@code gci-i20mr-merge-review-findings-20261015} finding 9). HotSpot 25 G1
 * (no {@code ExplicitGCInvokesConcurrent}) runs ONE stop-the-world
 * {@code Pause Full}: {@code G1 Old Generation} +1, {@code G1 Concurrent GC}
 * +0, and every heap pool's collection usage stamped. CratonVM booked the
 * stop-the-world full mark's remark as a {@code G1 Concurrent GC} pause.
 *
 * <pre>
 *   java     -XX:+UseG1GC -Xmx256m -cp &lt;classes&gt; GciOb30SysGcBeansProbe
 *   cratonvm -XX:+UseG1GC -Xmx256m -cp &lt;classes&gt; GciOb30SysGcBeansProbe
 * </pre>
 *
 * Expected stdout (HotSpot 25 G1, deterministic): {@code concurrent delta 0},
 * one {@code pool <name> collection-usage-stamped <committed > 0>} line per
 * heap pool in reverse name order (as HotSpot answers them), {@code done}.
 * (On Generational: {@code concurrent delta none}, then the Serial pools.)
 * stderr: {@code [ob30] <bean> <count0>-><count1>} per bean (the young / old
 * deltas are recorded, not judged: the default G1 System.gc() here is a young
 * pause plus the mark, HotSpot's one Pause Full only under
 * CRATONVM_G1_FULL_GC_ONE_REPORT).
 */
public class GciOb30SysGcBeansProbe {
    public static void main(String[] args) throws Exception {
        // Allocate a little so every pool has had a use.
        byte[][] keep = new byte[64][];
        for (int i = 0; i < keep.length; i++) {
            keep[i] = new byte[16 * 1024];
        }
        TreeMap<String, Long> before = new TreeMap<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            before.put(gc.getName(), gc.getCollectionCount());
        }
        System.gc();
        Thread.sleep(500);
        String concurrent = "none";
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long b = before.getOrDefault(gc.getName(), 0L);
            long a = gc.getCollectionCount();
            System.err.println("[ob30] " + gc.getName() + " " + b + "->" + a);
            if (gc.getName().equals("G1 Concurrent GC")) {
                concurrent = Long.toString(a - b);
            }
        }
        System.out.println("concurrent delta " + concurrent);
        TreeMap<String, Boolean> stamped = new TreeMap<>();
        for (MemoryPoolMXBean pool : ManagementFactory.getMemoryPoolMXBeans()) {
            if (pool.getType() != MemoryType.HEAP) {
                continue;
            }
            MemoryUsage cu = pool.getCollectionUsage();
            stamped.put(pool.getName(), cu != null && cu.getCommitted() > 0);
        }
        for (String name : stamped.descendingKeySet()) {
            System.out.println("pool " + name + " collection-usage-stamped " + stamped.get(name));
        }
        System.out.println("done" + (keep[0].length > 0 ? "" : "!"));
    }
}
