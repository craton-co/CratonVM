// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.Set;
import java.util.TreeSet;
import java.util.concurrent.ConcurrentSkipListSet;
import javax.management.NotificationEmitter;

/**
 * zr z1/tr (2026-10-12): which {@code GCCause} an ALLOCATION-driven ZGC
 * collection is reported under, on GC notifications (and, under
 * {@code -Xlog:gc}, on the {@code Major Collection (<cause>)} lines).
 *
 * <p>HotSpot 25 ZGC's director names a collection {@code Warmup},
 * {@code Allocation Rate}, {@code Proactive}, {@code High Usage},
 * {@code Timer} or {@code Allocation Stall}; it never says
 * {@code Allocation Failure} (that is Serial's / G1's young-pause cause).
 * CratonVM's ZGC said {@code Allocation Failure} on every allocation-driven
 * collection until this round.
 *
 * <p>Allocates {@code rounds} x 64 MiB of short-lived {@code byte[4096]}
 * garbage against a 4 MiB live set, never calls {@code System.gc()}, waits
 * up to 5 s for the notifications to drain, then prints:
 * <ul>
 * <li>stdout: {@code allocation_failure_seen=<bool>} and
 *     {@code unknown_zgc_cause_seen=<bool>} (a cause outside HotSpot ZGC's set),
 *     and {@code done} -- deterministic, SAME against HotSpot is the pass;</li>
 * <li>stderr: {@code [probe] cause <bean>|<action>|<cause>} per distinct
 *     triple (HotSpot also shows {@code ZGC Minor ...} rows: its young
 *     generation collects; CratonVM's default mode does not).</li>
 * </ul>
 */
public final class Zr1TrCauseProbe {
    static final Set<String> HOTSPOT_ZGC_CAUSES = Set.of(
            "Warmup", "Allocation Rate", "Proactive", "High Usage", "Timer",
            "Allocation Stall", "System.gc()", "Metadata GC Threshold", "No GC");

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        Set<String> seen = new ConcurrentSkipListSet<>();
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            ((NotificationEmitter) gc).addNotificationListener((n, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    GarbageCollectionNotificationInfo info = GarbageCollectionNotificationInfo
                            .from((javax.management.openmbean.CompositeData) n.getUserData());
                    seen.add(info.getGcName() + "|" + info.getGcAction() + "|" + info.getGcCause());
                }
            }, null, null);
        }
        byte[][] live = new byte[1024][];
        for (int i = 0; i < live.length; i++) {
            live[i] = new byte[4096];
        }
        long sink = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 16 * 1024; i++) {
                byte[] b = new byte[4096];
                b[i & 4095] = (byte) i;
                sink += b[(i * 7) & 4095];
            }
        }
        long deadline = System.nanoTime() + 5_000_000_000L;
        while (seen.isEmpty() && System.nanoTime() < deadline) {
            Thread.sleep(50);
        }
        Thread.sleep(200);
        boolean allocationFailure = false;
        boolean unknown = false;
        for (String t : new TreeSet<>(seen)) {
            System.err.println("[probe] cause " + t);
            String cause = t.substring(t.lastIndexOf('|') + 1);
            if (t.startsWith("ZGC")) {
                allocationFailure |= cause.equals("Allocation Failure");
                unknown |= !HOTSPOT_ZGC_CAUSES.contains(cause);
            }
        }
        System.err.println("[probe] distinct=" + seen.size() + " sink=" + sink + " live=" + live.length);
        System.out.println("allocation_failure_seen=" + allocationFailure);
        System.out.println("unknown_zgc_cause_seen=" + unknown);
        System.out.println("done");
    }
}
