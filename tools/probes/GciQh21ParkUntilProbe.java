// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.locks.LockSupport;

/**
 * gci i21/qh: `Unsafe.park`'s deadline arithmetic against HotSpot 25's
 * `Parker::park`. Every park below must return at once (an elapsed or
 * non-positive deadline); before i21 an absolute `0` (`parkUntil(0)`) parked
 * this VM forever, and an absolute `Long.MIN_VALUE` overflowed. (The
 * MIN_VALUE park is a platform thread's only: see the virtual block.)
 *
 *   GciQh21ParkUntilProbe
 *
 * stdout (deterministic, the same on HotSpot):
 *   qh21 platform parks=6 fast=true
 *   qh21 virtual parks=3 fast=true
 * stderr: the measured milliseconds.
 */
public class GciQh21ParkUntilProbe {
    static long platformParks() {
        LockSupport.parkUntil(0L);
        LockSupport.parkUntil(Long.MIN_VALUE);
        LockSupport.parkUntil(-1L);
        LockSupport.parkUntil(System.currentTimeMillis() - 5L);
        LockSupport.parkNanos(-1L);
        LockSupport.parkNanos(Long.MIN_VALUE);
        return 6;
    }

    public static void main(String[] args) throws Exception {
        long t0 = System.nanoTime();
        long n = platformParks();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.err.println("[qh21] platform ms=" + ms);
        System.out.println("qh21 platform parks=" + n + " fast=" + (ms < 5000L));

        final long[] vms = new long[1];
        Thread vt = Thread.ofVirtual().name("qh21-vt").start(() -> {
            long s = System.nanoTime();
            LockSupport.parkUntil(0L);
            // gci i22/rv (finding 2 of gci-i22rv-i21-default-path-review-findings):
            // NOT Long.MIN_VALUE here. A virtual thread's parkUntil computes
            // `deadline - currentTimeMillis()`, which wraps to a huge POSITIVE
            // delay for MIN_VALUE, and HotSpot then parks it ~292 years: the
            // row hung on HotSpot too. -1 is a non-positive deadline that
            // stays negative after the subtraction.
            LockSupport.parkUntil(-1L);
            LockSupport.parkUntil(System.currentTimeMillis() - 5L);
            vms[0] = (System.nanoTime() - s) / 1_000_000L;
        });
        vt.join();
        System.err.println("[qh21] virtual ms=" + vms[0]);
        System.out.println("qh21 virtual parks=3 fast=" + (vms[0] < 5000L));
    }
}
