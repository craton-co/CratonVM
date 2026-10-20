// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

/**
 * zr z1/tr (2026-10-12): {@code -XX:SoftMaxHeapSize} on ZGC, and the
 * {@code gc,init} block's {@code Soft Max Capacity:} / {@code Uncommit:} lines.
 *
 * <p>Allocates {@code byte[4096]} garbage (nothing retained) one MiB at a
 * time until the first collection is visible on the GC beans, then prints
 * how many MiB that took. HotSpot's warm-up rule fires at 10% of the SOFT
 * maximum, so {@code -Xmx512m -XX:SoftMaxHeapSize=64m} collects after
 * roughly 6 MiB where plain {@code -Xmx512m} collects after roughly 51 MiB.
 *
 * <p>stdout (deterministic): {@code first_gc_bucket=<low|high>} -- {@code low}
 * when the first collection came within the first 20 MiB -- and {@code done}.
 * stderr: {@code [probe] first_gc_after_mib=<n> max=<maxMemory>}.
 */
public final class Zr1TrSoftMaxProbe {
    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, gc.getCollectionCount());
        }
        return n;
    }

    public static void main(String[] args) {
        long base = gcCount();
        long sink = 0;
        int mib = 0;
        while (gcCount() == base && mib < 400) {
            for (int i = 0; i < 256; i++) {
                byte[] b = new byte[4096];
                b[i] = (byte) i;
                sink += b[(i * 3) & 4095];
            }
            mib++;
        }
        System.err.println("[probe] first_gc_after_mib=" + mib + " max=" + Runtime.getRuntime().maxMemory()
                + " sink=" + sink);
        System.out.println("first_gc_bucket=" + (mib <= 20 ? "low" : "high"));
        System.out.println("done");
    }
}
