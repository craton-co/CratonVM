// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 27, lane L4 (review of wave 26's pinning): ONE
// `LockSupport.unpark` gives ONE permit. Since wave 26 a virtual thread that
// parks under a native that re-entered Java (here `Method.invoke`) is PINNED
// and parks on its carrier's `ParkState`; `unpark` of a virtual thread both
// records the scheduler's sticky permit (`unpark_virtual`) and signals that
// `ParkState`. Page:
// docs/internal/fixed-bugs/interpreter-L4-an-unpark-of-a-pinned-virtual-thread-leaves-a-second-permit-FIXED-20260929.md
//
//   A: the pinned park consumes the unpark; the virtual thread's NEXT park
//      (an unmounting `parkNanos(400 ms)`, no unpark) must wait it out.
//   B: the mirror image: an unmounting park consumes the unpark; the next
//      park, pinned under `Method.invoke`, must wait out its 400 ms.
//   C, D (wave 28): the unpark lands while the thread RUNS; its first park
//      (C pinned, D unmounting) returns at once, the second (the other kind)
//      must wait out its 400 ms.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W27PinnedParkPermits
//
// HotSpot 25 (25.0.3, default and -Xint) prints:
//   A pinned park then free park waited: true
//   B free park then pinned park waited: true
//   C early unpark, pinned park returned at once: true, free park waited: true
//   D early unpark, free park returned at once: true, pinned park waited: true
//
// CratonVM at faa212874 / d8a690353 (read from the code, not run): `false` for
// A and B, and `true, false` for C and D (the second permit ends the second
// park at once). Fixed in wave 28 (`begin_carrier_park` / `end_carrier_park`,
// `unpark_virtual_waking`).
import java.lang.reflect.Method;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.LockSupport;

public class L4W27PinnedParkPermits {
    static volatile boolean parked;

    public static void parkOnce() {
        parked = true;
        LockSupport.park();
    }

    public static boolean parkTimed() {
        long t0 = System.nanoTime();
        LockSupport.parkNanos(TimeUnit.MILLISECONDS.toNanos(400));
        return System.nanoTime() - t0 >= TimeUnit.MILLISECONDS.toNanos(300);
    }

    static void awaitParked(Thread t) throws InterruptedException {
        while (!parked || t.getState() != Thread.State.WAITING) {
            Thread.sleep(5);
        }
        parked = false;
    }

    public static void main(String[] args) throws Throwable {
        Method parkOnce = L4W27PinnedParkPermits.class.getMethod("parkOnce");
        Method parkTimed = L4W27PinnedParkPermits.class.getMethod("parkTimed");
        boolean[] a = new boolean[1];
        Thread ta = Thread.ofVirtual().start(() -> {
            try {
                parkOnce.invoke(null); // pinned under the reflection native
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
            a[0] = parkTimed(); // unmounts; nothing unparks it
        });
        awaitParked(ta);
        LockSupport.unpark(ta);
        ta.join();
        System.out.println("A pinned park then free park waited: " + a[0]);

        boolean[] b = new boolean[1];
        Thread tb = Thread.ofVirtual().start(() -> {
            parkOnce(); // unmounts
            try {
                b[0] = (Boolean) parkTimed.invoke(null); // pinned; nothing unparks it
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
        });
        awaitParked(tb);
        LockSupport.unpark(tb);
        tb.join();
        System.out.println("B free park then pinned park waited: " + b[0]);

        // Wave 28: an unpark that lands while the thread RUNS is one permit
        // too. C: the pinned park takes it and returns at once; the next
        // (unmounting) park waits. D: the mirror image.
        Method parkUntimedNanos = L4W27PinnedParkPermits.class.getMethod("parkUntimedNanos");
        boolean[] c = new boolean[2];
        Thread tc = Thread.ofVirtual().start(() -> {
            while (!unparked) {
                Thread.onSpinWait();
            }
            try {
                long ns = (Long) parkUntimedNanos.invoke(null); // pinned; the permit ends it
                c[0] = ns < TimeUnit.MILLISECONDS.toNanos(300);
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
            c[1] = parkTimed(); // unmounts; nothing unparks it
        });
        LockSupport.unpark(tc);
        unparked = true;
        tc.join();
        unparked = false;
        System.out.println("C early unpark, pinned park returned at once: " + c[0] + ", free park waited: " + c[1]);

        boolean[] d = new boolean[2];
        Thread td = Thread.ofVirtual().start(() -> {
            while (!unparked) {
                Thread.onSpinWait();
            }
            d[0] = parkUntimedNanos() < TimeUnit.MILLISECONDS.toNanos(300); // unmounts; the permit ends it
            try {
                d[1] = (Boolean) parkTimed.invoke(null); // pinned; nothing unparks it
            } catch (ReflectiveOperationException e) {
                throw new RuntimeException(e);
            }
        });
        LockSupport.unpark(td);
        unparked = true;
        td.join();
        System.out.println("D early unpark, free park returned at once: " + d[0] + ", pinned park waited: " + d[1]);
    }

    static volatile boolean unparked;

    public static long parkUntimedNanos() {
        long t0 = System.nanoTime();
        LockSupport.park();
        return System.nanoTime() - t0;
    }
}
