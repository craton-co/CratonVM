// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.management.GarbageCollectionNotificationInfo;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;
import javax.management.NotificationEmitter;

/**
 * gch h1/ob (2026-10-08): how long after a {@code System.gc()} the FIRST
 * {@code GarbageCollectionNotificationInfo} of the run arrives, then three more
 * ({@code docs/internal/gc/gcd-d10o-first-gc-notification-arrives-a-second-late-CLOSED-20261012.md}).
 *
 * <p>Registers one listener on every GC bean, then four times: {@code System.gc()}
 * and wait (up to 10 s) for a notification. stdout is deterministic:
 * {@code round <i> notified=true} per round and {@code done}; the latencies go
 * to stderr ({@code [probe] round=<i> first_ms=<t>}), with
 * {@code [probe] within_100ms=<n>/4}. Arg 0 (optional): the wait in ms before
 * the first {@code System.gc()} (default 0), so a row can tell a cold delivery
 * path from a delivery thread that has had time to start.
 *
 * <p>Pass rule (rows {@code ob1_notif_*}): stdout SAME as HotSpot 25; the
 * deciding figure is round 1's {@code first_ms} (HotSpot ~40 ms), read with
 * the {@code --verbose:gc} split {@code notif_first_wait_us} /
 * {@code notif_first_deliver_us} of the {@code [GC] gc_notifications:} line.
 */
public final class GchOb1NotifLatencyProbe {
    public static void main(String[] args) throws Exception {
        long settleMs = args.length > 0 ? Long.parseLong(args[0]) : 0;
        AtomicLong arrived = new AtomicLong();
        CountDownLatch[] latch = {new CountDownLatch(1)};
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            ((NotificationEmitter) gc).addNotificationListener((n, hb) -> {
                if (GarbageCollectionNotificationInfo.GARBAGE_COLLECTION_NOTIFICATION.equals(n.getType())) {
                    arrived.compareAndSet(0, System.nanoTime());
                    synchronized (latch) {
                        latch[0].countDown();
                    }
                }
            }, null, null);
        }
        if (settleMs > 0) {
            Thread.sleep(settleMs);
        }
        int within = 0;
        for (int round = 1; round <= 4; round++) {
            CountDownLatch l = new CountDownLatch(1);
            synchronized (latch) {
                latch[0] = l;
            }
            arrived.set(0);
            // gch h2/ob: also from BEFORE the call. A delivery made inline
            // (before System.gc() returns) printed first_ms=0 and hid its cost;
            // call_ms covers the collection plus the delivery either way.
            // gci i1/ob: a census marker. No stderr line of this probe said
            // "System.gc()", so vh4-fl D3's window (from the first such line to
            // `[probe] round=1`) was never opened and read 0 class loads by
            // construction. Printed without string concatenation, which would
            // spin LambdaForm classes of its own here.
            System.err.print("[probe] gc round=");
            System.err.println(round);
            long tCall = System.nanoTime();
            System.gc();
            long t0 = System.nanoTime();
            boolean ok = l.await(10, TimeUnit.SECONDS);
            long at = arrived.get();
            long ms = ok && at != 0 ? Math.max(0, (at - t0) / 1_000_000) : -1;
            long callMs = ok && at != 0 ? (at - tCall) / 1_000_000 : -1;
            if (ok && ms <= 100) {
                within++;
            }
            System.err.println("[probe] round=" + round + " first_ms=" + ms + " call_ms=" + callMs
                    + " gc_call_ms=" + (t0 - tCall) / 1_000_000);
            System.out.println("round " + round + " notified=" + ok);
        }
        System.err.println("[probe] within_100ms=" + within + "/4");
        System.out.println("done");
    }
}
