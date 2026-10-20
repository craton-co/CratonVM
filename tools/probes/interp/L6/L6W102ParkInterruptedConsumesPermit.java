// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 102, lane L6 -- a park with BOTH a pending
// `unpark` permit and a pending interrupt.
//
// HotSpot 25's `Parker::park` starts with `xchg(&_counter, 0)` (the permit is
// taken) and only then returns for the interrupt. So after that park and
// `Thread.interrupted()`, the next `parkNanos` has no permit and waits out
// its timeout. CratonVM's park returned for the interrupt FIRST, keeping the
// permit, and the next `parkNanos(300 ms)` returned at once
// (`second park waited: false`).
//
// Run: java -cp out L6W102ParkInterruptedConsumesPermit
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// first park returned, interrupted: true
// second park waited: true
// HOTSPOT_EXPECTED_END

import java.util.concurrent.locks.LockSupport;

public class L6W102ParkInterruptedConsumesPermit {
    public static void main(String[] args) throws Exception {
        Thread t = new Thread(() -> {
            Thread me = Thread.currentThread();
            LockSupport.unpark(me);
            me.interrupt();
            LockSupport.park();
            System.out.println("first park returned, interrupted: " + Thread.interrupted());
            long t0 = System.nanoTime();
            LockSupport.parkNanos(300_000_000L);
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("second park waited: " + (ms >= 200));
        });
        t.start();
        t.join();
    }
}
