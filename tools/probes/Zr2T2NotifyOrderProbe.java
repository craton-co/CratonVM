// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import javax.management.NotificationEmitter;

/**
 * zr z2/t2 (2026-10-12): the ORDER of the GC notifications one
 * {@code System.gc()} produces, the heap pools' manager lists, and where a
 * fresh allocation is booked (young or old pool).
 *
 * <p>HotSpot 25 ZGC closes each pause tracer inside its cycle tracer, so a
 * listener receives every {@code end of GC pause} before the
 * {@code end of GC cycle} it belongs to. CratonVM queued them in bean order
 * (cycle first) until {@code CRATONVM_ZGC_HOTSPOT_BEAN_ORDER}.
 *
 * <p>stdout (deterministic; SAME against {@code java -XX:+UseZGC} is the
 * goal, the pause COUNT per cycle is a known structural difference -- see the
 * z2-t2 report):
 * <ul>
 * <li>{@code pool <name> managers=[...]} for each heap pool;</li>
 * <li>{@code last=<bean>|<action>} -- the final notification of the
 *     {@code System.gc()} (HotSpot: the cycle);</li>
 * <li>{@code pause_before_cycle=<bool>} -- every pause notification arrived
 *     before the first cycle notification;</li>
 * <li>{@code fresh_in_young=<bool>} -- an 8 MiB array allocated after the
 *     collection raised the young pool's used by at least 8 MiB and the old
 *     pool's by less;</li>
 * <li>{@code done}.</li>
 * </ul>
 * stderr: {@code [probe] seq ...} the raw sequence.
 */
public final class Zr2T2NotifyOrderProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        List<String> seq = Collections.synchronizedList(new ArrayList<>());
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            ((NotificationEmitter) gc).addNotificationListener((n, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    GarbageCollectionNotificationInfo info = GarbageCollectionNotificationInfo
                            .from((javax.management.openmbean.CompositeData) n.getUserData());
                    if ("System.gc()".equals(info.getGcCause())) {
                        seq.add(info.getGcName() + "|" + info.getGcAction());
                    }
                }
            }, null, null);
        }
        MemoryPoolMXBean young = null;
        MemoryPoolMXBean old = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getType() != MemoryType.HEAP) {
                continue;
            }
            System.out.println("pool " + p.getName() + " managers=" + Arrays.toString(p.getMemoryManagerNames()));
            if (p.getName().contains("Young")) {
                young = p;
            } else if (p.getName().contains("Old")) {
                old = p;
            }
        }
        System.gc();
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (seq.isEmpty() && System.nanoTime() < deadline) {
            Thread.sleep(50);
        }
        Thread.sleep(500);
        List<String> got;
        synchronized (seq) {
            got = new ArrayList<>(seq);
        }
        System.err.println("[probe] seq " + got);
        int firstCycle = -1;
        int lastPause = -1;
        for (int i = 0; i < got.size(); i++) {
            if (got.get(i).endsWith("end of GC cycle") && firstCycle < 0) {
                firstCycle = i;
            }
            if (got.get(i).endsWith("end of GC pause")) {
                lastPause = i;
            }
        }
        System.out.println("last=" + (got.isEmpty() ? "NONE" : got.get(got.size() - 1)));
        System.out.println("pause_before_cycle=" + (firstCycle >= 0 && lastPause >= 0 && lastPause < firstCycle));
        if (young != null && old != null) {
            MemoryUsage y0 = young.getUsage();
            MemoryUsage o0 = old.getUsage();
            sink = new byte[8 * 1024 * 1024];
            MemoryUsage y1 = young.getUsage();
            MemoryUsage o1 = old.getUsage();
            long dy = y1.getUsed() - y0.getUsed();
            long dold = o1.getUsed() - o0.getUsed();
            System.err.println("[probe] young " + y0.getUsed() + "->" + y1.getUsed()
                    + " old " + o0.getUsed() + "->" + o1.getUsed());
            System.out.println("fresh_in_young=" + (dy >= 8L * 1024 * 1024 && dold < 8L * 1024 * 1024));
        } else {
            System.out.println("fresh_in_young=no-pools");
        }
        System.out.println("done");
    }
}
