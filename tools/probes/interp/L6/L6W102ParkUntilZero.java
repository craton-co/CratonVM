// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L6 -- `LockSupport.parkUntil(0)`.
//
// `parkUntil(deadline)` calls `Unsafe.park(true, deadline)` with no guard.
// HotSpot 25's `Parker::park` takes the permit, then
// `if (time < 0 || (isAbsolute && time == 0)) return;` -- so an absolute
// deadline of 0 (the epoch, long past) returns at once. CratonVM's
// `native_unsafe_park` read `time == 0` as "park indefinitely" before
// looking at `isAbsolute`, so `parkUntil(0)` parked forever (the worker
// below never finished; the probe printed `parkUntil(0) returned: false`).
// Also covered (already HotSpot's answer, kept as a guard): `parkUntil` of
// a past deadline, `parkNanos` of 0 and of a negative time, and a park with
// a pending permit.
//
// Run: java -cp out L6W102ParkUntilZero
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// parkUntil(0) returned: true
// parkUntil(past) returned: true
// parkNanos(0) returned: true
// parkNanos(-1) returned: true
// park with permit returned: true
// virtual parkUntil(0) returned: true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.locks.LockSupport;

public class L6W102ParkUntilZero {
    static boolean returns(boolean virtual, Runnable r) throws InterruptedException {
        Thread t = virtual ? Thread.ofVirtual().unstarted(r) : new Thread(r);
        t.setDaemon(true);
        t.start();
        t.join(5_000);
        return !t.isAlive();
    }

    public static void main(String[] args) throws Exception {
        System.out.println("parkUntil(0) returned: "
                + returns(false, () -> LockSupport.parkUntil(0L)));
        System.out.println("parkUntil(past) returned: "
                + returns(false, () -> LockSupport.parkUntil(System.currentTimeMillis() - 1_000)));
        System.out.println("parkNanos(0) returned: "
                + returns(false, () -> LockSupport.parkNanos(0L)));
        System.out.println("parkNanos(-1) returned: "
                + returns(false, () -> LockSupport.parkNanos(-1L)));
        System.out.println("park with permit returned: " + returns(false, () -> {
            LockSupport.unpark(Thread.currentThread());
            LockSupport.park();
        }));
        System.out.println("virtual parkUntil(0) returned: "
                + returns(true, () -> LockSupport.parkUntil(0L)));
    }
}
