// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;

/**
 * zs s2/ob (2026-10-14): HotSpot 25 ZGC aligns {@code -Xmx}, {@code -Xms} and
 * {@code MinHeapSize} up to its 2 MiB granule ({@code -Xmx9m} is a 10M heap,
 * {@code -Xmx100000k} a 98M one, {@code -Xms5000k} a 6M initial heap; local
 * 25.0.3 {@code -Xlog:gc+init}: {@code Max Capacity: 10M}, {@code Min
 * Capacity: 6M}, {@code Initial Capacity: 6M}). This probe prints the
 * Java-visible consequences, which are deterministic for a given command line:
 * {@code Runtime.maxMemory()}, the heap {@code MemoryUsage} max and init, and
 * each heap pool's max.
 *
 * <p>Rows: {@code -XX:+UseZGC -Xmx9m -Xms5000k Zs2ObHeapAlign} and
 * {@code -XX:+UseZGC -Xmx100000k Zs2ObHeapAlign} (the latter's init is the
 * ergonomic initial heap, which depends on physical memory: printed only as
 * {@code init_le_max}).
 */
public final class Zs2ObHeapAlign {
    public static void main(String[] args) {
        long max = Runtime.getRuntime().maxMemory();
        var heap = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        System.out.println("rt_max=" + max);
        System.out.println("heap_max=" + heap.getMax() + " init_le_max=" + (heap.getInit() <= heap.getMax())
                + " init_is_2m_multiple=" + (heap.getInit() % (2L << 20) == 0));
        System.err.println("[probe] heap " + heap);
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() == MemoryType.HEAP) {
                System.out.println("pool " + p.getName() + " max=" + p.getUsage().getMax());
            }
        }
        System.out.println("done");
    }
}
