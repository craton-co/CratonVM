// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.List;
import java.util.concurrent.atomic.AtomicLong;
import javax.management.NotificationEmitter;
import javax.management.openmbean.CompositeData;

/**
 * gen r5w3/obs7 (2026-09-26): {@code GarbageCollectorMXBean.getCollectionTime()}
 * is the accumulated pause time in whole milliseconds of the SUM, as HotSpot's
 * {@code GCMemoryManager::_accumulated_timer} -- not one rounded-up millisecond
 * per collection. Page:
 * {@code docs/internal/gc/gengc-r5w2-obs6-collection-time-ceils-every-pause-FIXED-20260927.md}.
 *
 * <p>Runs {@code N} (default 400) collections of a small young generation and
 * compares the beans' summed time delta {@code dt} against:
 * <ol>
 *   <li>{@code collection-time-tracks-pauses}: the summed
 *       {@code GcInfo.getDuration()} of the GC notifications received for the
 *       same collections, {@code sumDur}: {@code dt <= sumDur + n/2 + 2}. Each
 *       duration is a whole-millisecond difference, so {@code sumDur} tracks
 *       the true pause sum within rounding noise; a VM charging every pause a
 *       full millisecond reads {@code dt >= n}, which breaks the bound as soon
 *       as the average pause is under half a millisecond;
 *   <li>{@code collection-time-within-wall}: {@code dt <= wall_ms + 1} -- the
 *       pauses happened inside the measured wall time. The per-pause ceiling
 *       breaks this once more than one collection completes per millisecond.
 * </ol>
 * Deterministic stdout on HotSpot Serial:
 * <pre>
 *   collections-counted=true
 *   collection-time-tracks-pauses=true
 *   collection-time-within-wall=true
 * </pre>
 * The numbers go to stderr ({@code [probe] collections=.. time_ms=..
 * notified=.. sum_duration_ms=.. wall_ms=..}). Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -Xmn2m -cp tools/bench GenR5W3CollectionTimeProbe 400
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx64m -Xmn2m -cp tools/bench GenR5W3CollectionTimeProbe 400
 * </pre>
 * The two checks only DISCRIMINATE when pauses are short (sub-millisecond on
 * average) or frequent; the arithmetic itself is pinned by
 * {@code gc/src/gc_metrics.rs::collection_time_is_the_floor_of_the_microsecond_sum}.
 */
public final class GenR5W3CollectionTimeProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int target = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        List<GarbageCollectorMXBean> gcs = ManagementFactory.getGarbageCollectorMXBeans();
        AtomicLong notified = new AtomicLong();
        AtomicLong sumDuration = new AtomicLong();
        for (GarbageCollectorMXBean gc : gcs) {
            ((NotificationEmitter) gc).addNotificationListener((n, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    GarbageCollectionNotificationInfo info =
                        GarbageCollectionNotificationInfo.from((CompositeData) n.getUserData());
                    notified.incrementAndGet();
                    sumDuration.addAndGet(info.getGcInfo().getDuration());
                }
            }, null, null);
        }
        // Warm the allocation path so the measured window is steady state.
        churn(gcs, 20);

        long c0 = count(gcs);
        long t0 = time(gcs);
        long n0 = notified.get();
        long d0 = sumDuration.get();
        long w0 = System.nanoTime();
        churn(gcs, target);
        long wallMs = (System.nanoTime() - w0) / 1_000_000;
        long dc = count(gcs) - c0;
        long dt = time(gcs) - t0;
        // Notifications are delivered asynchronously on HotSpot: give the
        // last few a moment to arrive.
        long deadline = System.nanoTime() + 2_000_000_000L;
        while (notified.get() - n0 < dc && System.nanoTime() < deadline) {
            Thread.sleep(10);
        }
        long dn = notified.get() - n0;
        long dd = sumDuration.get() - d0;

        System.err.println("[probe] collections=" + dc + " time_ms=" + dt + " notified=" + dn
            + " sum_duration_ms=" + dd + " wall_ms=" + wallMs);
        System.out.println("collections-counted=" + (dc >= target));
        System.out.println("collection-time-tracks-pauses=" + (dt <= dd + dc / 2 + 2));
        System.out.println("collection-time-within-wall=" + (dt <= wallMs + 1));
    }

    /** Allocate short-lived garbage until {@code n} more collections completed. */
    static void churn(List<GarbageCollectorMXBean> gcs, int n) {
        long start = count(gcs);
        while (count(gcs) - start < n) {
            for (int i = 0; i < 1024; i++) {
                sink = new byte[256];
            }
        }
    }

    static long count(List<GarbageCollectorMXBean> gcs) {
        long c = 0;
        for (GarbageCollectorMXBean gc : gcs) {
            c += Math.max(0, gc.getCollectionCount());
        }
        return c;
    }

    static long time(List<GarbageCollectorMXBean> gcs) {
        long t = 0;
        for (GarbageCollectorMXBean gc : gcs) {
            t += Math.max(0, gc.getCollectionTime());
        }
        return t;
    }
}
