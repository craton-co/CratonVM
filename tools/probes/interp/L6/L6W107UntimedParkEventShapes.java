// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 107, lane L6 -- the Java-visible shapes of an
// UNTIMED `LockSupport.park()` on a platform thread, for the A/B of
// `CRATONVM_PARK_UNTIMED_EVENT` (default off; Windows only: the untimed park
// waits on the thread's wake event, as HotSpot's Windows `Parker`, instead of
// the condvar). Pages: docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md
// and interpreter-i91-L6-virtual-thread-code-is-never-compiled-FIXED-20261017.md ("Wave 107").
//
// Each row must print the same with the switch off and on, in both modes,
// JIT and --nojit:
//   * unpark wakes a parked thread; a permit given before the park makes it
//     return at once;
//   * an interrupt wakes it and leaves the flag set;
//   * unparking one parked thread leaves another parked thread parked (each
//     thread's event is its own);
//   * after an untimed park ended by unpark, the thread's next timed park
//     waits (no signal is left on the event for it to wake on);
//   * 2000 unpark/park ping-pong hand-offs complete.
//
// Run: java -cp out L6W107UntimedParkEventShapes
//      CRATONVM_PARK_UNTIMED_EVENT=1 cratonvm L6W107UntimedParkEventShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// unpark wakes: true
// permit first returns at once: true
// interrupt wakes: true flag=true
// unwoken thread still parked: true
// timed park after unpark waited: true
// ping-pong handoffs=2000 ok=true
// done
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W107UntimedParkEventShapes {
    static volatile boolean go;
    static volatile int turn;

    static void waitParked(Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (t.getState() != Thread.State.WAITING && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        Thread.sleep(20);
    }

    public static void main(String[] args) throws Exception {
        // 1. unpark wakes.
        go = false;
        Thread a = new Thread(() -> {
            while (!go) {
                LockSupport.park();
            }
        });
        a.start();
        waitParked(a);
        go = true;
        LockSupport.unpark(a);
        a.join(10_000);
        System.out.println("unpark wakes: " + !a.isAlive());

        // 2. a permit given first.
        boolean[] fast = new boolean[1];
        Thread b = new Thread(() -> {
            LockSupport.unpark(Thread.currentThread());
            long t0 = System.nanoTime();
            LockSupport.park();
            fast[0] = System.nanoTime() - t0 < 1_000_000_000L;
        });
        b.start();
        b.join(10_000);
        System.out.println("permit first returns at once: " + fast[0]);

        // 3. interrupt wakes, flag stays set.
        boolean[] flag = new boolean[1];
        Thread c = new Thread(() -> {
            LockSupport.park();
            flag[0] = Thread.currentThread().isInterrupted();
        });
        c.start();
        waitParked(c);
        c.interrupt();
        c.join(10_000);
        System.out.println("interrupt wakes: " + !c.isAlive() + " flag=" + flag[0]);

        // 4. a thread nobody unparks stays parked while another is unparked.
        go = false;
        Thread d = new Thread(() -> {
            while (!go) {
                LockSupport.park();
            }
        });
        Thread e = new Thread(() -> LockSupport.park());
        d.start();
        e.start();
        waitParked(d);
        waitParked(e);
        LockSupport.unpark(e);
        e.join(10_000);
        Thread.sleep(50);
        System.out.println("unwoken thread still parked: " + (d.getState() == Thread.State.WAITING));
        go = true;
        LockSupport.unpark(d);
        d.join(10_000);

        // 5. the next timed park after an untimed one ended by unpark waits.
        long[] waited = new long[1];
        go = false;
        Thread f = new Thread(() -> {
            while (!go) {
                LockSupport.park();
            }
            long t0 = System.nanoTime();
            LockSupport.parkNanos(100_000_000L);
            waited[0] = System.nanoTime() - t0;
        });
        f.start();
        waitParked(f);
        go = true;
        LockSupport.unpark(f);
        f.join(10_000);
        System.out.println("timed park after unpark waited: " + (waited[0] >= 50_000_000L));

        // 6. ping-pong.
        final int rounds = 2000;
        AtomicInteger done = new AtomicInteger();
        Thread[] pair = new Thread[2];
        turn = 0;
        for (int k = 0; k < 2; k++) {
            final int me = k;
            pair[k] = new Thread(() -> {
                for (int i = 0; i < rounds / 2; i++) {
                    while (turn != me) {
                        LockSupport.park();
                    }
                    turn = 1 - me;
                    done.incrementAndGet();
                    LockSupport.unpark(pair[1 - me]);
                }
            });
        }
        pair[0].start();
        pair[1].start();
        pair[0].join(60_000);
        pair[1].join(60_000);
        System.out.println("ping-pong handoffs=" + done.get() + " ok=" + (done.get() == rounds));
        System.out.println("done");
    }
}
