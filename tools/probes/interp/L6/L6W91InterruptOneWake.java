// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 91, lane L6 -- behaviour probe for
// docs/internal/fixed-bugs/interpreter-i77-L6-proposal-one-trip-interrupt-wake-DONE-20261127.md
// (CRATONVM_INTERRUPT_ONE_WAKE: Thread.interrupt wakes its target's park state
// in one locked trip) and
// docs/internal/fixed-bugs/interpreter-i77-L6-proposal-every-vm-unpark-through-the-shape-DONE-20261127.md
// (the carrier-park wake of a virtual thread takes the VM's unpark shape).
//
// Every row must read the same with the switch off and on, in both modes:
//   javac -d out tools/probes/interp/L6/L6W91InterruptOneWake.java
//   java -cp out L6W91InterruptOneWake
//   cratonvm -cp out L6W91InterruptOneWake
//   CRATONVM_INTERRUPT_ONE_WAKE=1 cratonvm -cp out L6W91InterruptOneWake
//   (each also with --compatible, and with CRATONVM_PARK_ATOMIC_PERMIT=1)
// Timing: L6W76InterruptBench (interrupt-running, interrupt-under-unpark).
//
// HOTSPOT_EXPECTED_BEGIN
// park: woke=true interrupted=true
// park-after-interrupt: returned=true interrupted=true
// sleep: InterruptedException interrupted=false
// wait: InterruptedException interrupted=false
// timed-park: woke=true interrupted=true
// sleep-with-leftover-permit: InterruptedException
// interrupt-running: seen=true
// vthread-park: woke=true interrupted=true
// vthread-unpark: woke=true
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.LockSupport;

public class L6W91InterruptOneWake {
    static final long BOUND_MS = 10_000;

    /** Waits until {@code t} is WAITING or TIMED_WAITING (or dead). */
    static void awaitBlocked(Thread t) throws InterruptedException {
        long end = System.currentTimeMillis() + BOUND_MS;
        while (System.currentTimeMillis() < end) {
            Thread.State s = t.getState();
            if (s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING
                    || s == Thread.State.TERMINATED) {
                Thread.sleep(20);
                return;
            }
            Thread.sleep(1);
        }
    }

    static void parkRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Thread t = new Thread(() -> {
            long end = System.currentTimeMillis() + BOUND_MS;
            while (!Thread.currentThread().isInterrupted() && System.currentTimeMillis() < end) {
                LockSupport.park();
            }
            out.set("park: woke=" + (System.currentTimeMillis() < end)
                    + " interrupted=" + Thread.currentThread().isInterrupted());
        });
        t.start();
        awaitBlocked(t);
        t.interrupt();
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    static volatile boolean raised;

    // Interpreter round i5 wave 102, lane L6 (page
    // `interpreter-i102-L6-l6w91-park-after-interrupt-row-races-FIXED-20261015`): the target used
    // to wait on a `CountDownLatch`, whose `await` is INTERRUPTIBLE. When the
    // target was already parked in it, `countDown`'s unpark and the
    // interrupt's unpark coalesced into one permit (HotSpot's `Parker` is
    // binary too), AQS turned the interrupt into an `InterruptedException`
    // (clearing the flag), and the row's own `park()` then had neither a permit
    // nor a flag: it never returned, `null` was printed and the non-daemon
    // target kept the process alive -- on HotSpot as well, in that
    // interleaving; when the interrupt landed before `await` started, HotSpot
    // printed `interrupted=false` (the `InterruptedException` path). A spin on a
    // volatile cannot consume the interrupt, so every interleaving answers the
    // same line.
    static void parkAfterInterruptRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        raised = false;
        Thread t = new Thread(() -> {
            while (!raised) {
                Thread.onSpinWait();
            }
            long t0 = System.nanoTime();
            LockSupport.park();
            long ms = (System.nanoTime() - t0) / 1_000_000;
            out.set("park-after-interrupt: returned=" + (ms < BOUND_MS)
                    + " interrupted=" + Thread.currentThread().isInterrupted());
        });
        t.setDaemon(true);
        t.start();
        raised = true;
        // Raise while the target may be anywhere before its park.
        t.interrupt();
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    static void sleepRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Thread t = new Thread(() -> {
            try {
                Thread.sleep(60_000);
                out.set("sleep: completed");
            } catch (InterruptedException e) {
                out.set("sleep: InterruptedException interrupted="
                        + Thread.currentThread().isInterrupted());
            }
        });
        t.start();
        awaitBlocked(t);
        t.interrupt();
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    static void waitRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Object lock = new Object();
        Thread t = new Thread(() -> {
            synchronized (lock) {
                try {
                    lock.wait();
                    out.set("wait: returned");
                } catch (InterruptedException e) {
                    out.set("wait: InterruptedException interrupted="
                            + Thread.currentThread().isInterrupted());
                }
            }
        });
        t.start();
        awaitBlocked(t);
        t.interrupt();
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    static void timedParkRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Thread t = new Thread(() -> {
            long end = System.currentTimeMillis() + BOUND_MS;
            while (!Thread.currentThread().isInterrupted() && System.currentTimeMillis() < end) {
                LockSupport.parkNanos(TimeUnit.SECONDS.toNanos(60));
            }
            out.set("timed-park: woke=" + (System.currentTimeMillis() < end)
                    + " interrupted=" + Thread.currentThread().isInterrupted());
        });
        t.start();
        awaitBlocked(t);
        t.interrupt();
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    /** A sleeper whose park permit is already set must still see the interrupt. */
    static void sleepWithLeftoverPermitRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Thread t = new Thread(() -> {
            LockSupport.unpark(Thread.currentThread());
            try {
                Thread.sleep(60_000);
                out.set("sleep-with-leftover-permit: completed");
            } catch (InterruptedException e) {
                out.set("sleep-with-leftover-permit: InterruptedException");
            }
        });
        t.start();
        awaitBlocked(t);
        t.interrupt();
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    static volatile boolean stop;

    static void interruptRunningRow() throws Exception {
        stop = false;
        AtomicReference<Boolean> seen = new AtomicReference<>(false);
        Thread t = new Thread(() -> {
            while (!stop) {
                if (Thread.currentThread().isInterrupted()) {
                    seen.set(true);
                }
                Thread.onSpinWait();
            }
        });
        t.start();
        for (int i = 0; i < 10_000; i++) {
            t.interrupt();
        }
        long end = System.currentTimeMillis() + BOUND_MS;
        while (!seen.get() && System.currentTimeMillis() < end) {
            Thread.sleep(1);
        }
        stop = true;
        t.join(BOUND_MS);
        System.out.println("interrupt-running: seen=" + seen.get());
    }

    static void vthreadParkRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Thread t = Thread.ofVirtual().unstarted(() -> {
            long end = System.currentTimeMillis() + BOUND_MS;
            while (!Thread.currentThread().isInterrupted() && System.currentTimeMillis() < end) {
                LockSupport.park();
            }
            out.set("vthread-park: woke=" + (System.currentTimeMillis() < end)
                    + " interrupted=" + Thread.currentThread().isInterrupted());
        });
        t.start();
        awaitBlocked(t);
        t.interrupt();
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    static void vthreadUnparkRow() throws Exception {
        AtomicReference<String> out = new AtomicReference<>();
        Object lock = new Object();
        Thread t = Thread.ofVirtual().unstarted(() -> {
            long end = System.currentTimeMillis() + BOUND_MS;
            // Parked inside synchronized: JEP 491 unmounts, an older shape
            // pins the carrier; either way the unpark must end the park.
            synchronized (lock) {
                LockSupport.park();
            }
            out.set("vthread-unpark: woke=" + (System.currentTimeMillis() < end));
        });
        t.start();
        awaitBlocked(t);
        LockSupport.unpark(t);
        t.join(BOUND_MS);
        System.out.println(out.get());
    }

    public static void main(String[] args) throws Exception {
        parkRow();
        parkAfterInterruptRow();
        sleepRow();
        waitRow();
        timedParkRow();
        sleepWithLeftoverPermitRow();
        interruptRunningRow();
        vthreadParkRow();
        vthreadUnparkRow();
        System.out.println("done");
    }
}
