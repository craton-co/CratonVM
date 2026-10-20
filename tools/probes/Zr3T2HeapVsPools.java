// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;

/**
 * zr z3/t2 (2026-10-12): does {@code MemoryMXBean.getHeapMemoryUsage()} equal
 * the sum of the heap {@code MemoryPoolMXBean}s' usages? Review lane rv3
 * expected HotSpot's {@code jmm_GetMemoryUsage(true)} to sum the pools; lane
 * t2 reads it as answering {@code CollectedHeap::memory_usage()} (the heap's
 * capacity as committed) and summing only for non-heap. This row decides.
 *
 * <p>After a {@code System.gc()} with a small retained set, prints (stdout,
 * deterministic):
 * <ul>
 * <li>{@code used_equal=<bool>} -- heap used == sum of pool used, within 2 MiB
 *     (the two reads are separate samples);</li>
 * <li>{@code committed_equal=<bool>} -- heap committed == sum of pool
 *     committed (exact);</li>
 * <li>{@code committed_cmp=<lt|eq|gt>} -- pool sum versus heap committed;</li>
 * <li>{@code done}.</li>
 * </ul>
 * stderr: the raw figures.
 */
public final class Zr3T2HeapVsPools {
    static Object keep;

    public static void main(String[] args) {
        keep = new byte[1 << 20];
        System.gc();
        long poolUsed = 0;
        long poolCommitted = 0;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            MemoryUsage u = p.getUsage();
            System.err.println("[probe] pool " + p.getName() + " used=" + u.getUsed()
                    + " committed=" + u.getCommitted());
            poolUsed += u.getUsed();
            poolCommitted += u.getCommitted();
        }
        MemoryUsage heap = ManagementFactory.getMemoryMXBean().getHeapMemoryUsage();
        System.err.println("[probe] heap used=" + heap.getUsed() + " committed=" + heap.getCommitted()
                + " pools used=" + poolUsed + " committed=" + poolCommitted);
        System.out.println("used_equal=" + (Math.abs(heap.getUsed() - poolUsed) <= (2L << 20)));
        System.out.println("committed_equal=" + (heap.getCommitted() == poolCommitted));
        long c = Long.compare(poolCommitted, heap.getCommitted());
        System.out.println("committed_cmp=" + (c < 0 ? "lt" : c == 0 ? "eq" : "gt"));
        System.out.println("done");
    }
}
