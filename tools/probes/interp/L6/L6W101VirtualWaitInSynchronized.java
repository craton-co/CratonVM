// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L6 -- `Object.wait` inside
// `synchronized` on a virtual thread (JEP 491, JDK 24+): the virtual thread
// UNMOUNTS, so on a one-carrier scheduler another virtual thread can take
// the monitor and notify it. While it waits its state is WAITING (or
// TIMED_WAITING) and its `toString()` names no carrier ("/waiting", not
// "/waiting@ForkJoinPool-1-worker-1": `VirtualThread.toString` appends the
// carrier only while mounted). After the notify it re-acquires the monitor
// before `wait` returns (`holdsLock` true), and an interrupt during the
// wait is thrown only after the re-acquisition (`holdsLock` true in the
// catch).
//
// Run: java -cp out L6W101VirtualWaitInSynchronized (the scheduler size is
// set by the probe itself before the first virtual thread). A pinned wait
// cannot finish: the "finished" rows print false after a 10 s bound.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags; derived from the sources)
// waiter state: WAITING
// waiter toString: VirtualThread[#,waiter]/waiting
// notifier finished: true
// waiter finished: true
// waiter held the monitor after wait: true
// timed waiter state: TIMED_WAITING
// timed waiter toString: VirtualThread[#,timed]/timed_waiting
// timed waiter finished: true
// interrupted waiter: java.lang.InterruptedException holdsLock=true
// interrupted waiter interrupt status after catch: false
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicReference;

public class L6W101VirtualWaitInSynchronized {
    static final Object LOCK = new Object();
    static volatile boolean go;
    static volatile boolean held;

    static String strip(String s) {
        return s.replaceAll("#[0-9]+", "#");
    }

    static void awaitState(Thread t, Thread.State want) throws InterruptedException {
        for (int i = 0; i < 10_000 && t.getState() != want; i++) {
            Thread.sleep(1);
        }
    }

    public static void main(String[] args) throws Exception {
        System.setProperty("jdk.virtualThreadScheduler.parallelism", "1");
        System.setProperty("jdk.virtualThreadScheduler.maxPoolSize", "1");

        Thread waiter = Thread.ofVirtual().name("waiter").start(() -> {
            synchronized (LOCK) {
                try {
                    while (!go) {
                        LOCK.wait();
                    }
                } catch (InterruptedException e) {
                    return;
                }
                held = Thread.holdsLock(LOCK);
            }
        });
        awaitState(waiter, Thread.State.WAITING);
        System.out.println("waiter state: " + waiter.getState());
        System.out.println("waiter toString: " + strip(waiter.toString()));
        Thread notifier = Thread.ofVirtual().name("notifier").start(() -> {
            synchronized (LOCK) {
                go = true;
                LOCK.notifyAll();
            }
        });
        notifier.join(10_000);
        System.out.println("notifier finished: " + !notifier.isAlive());
        waiter.join(10_000);
        System.out.println("waiter finished: " + !waiter.isAlive());
        System.out.println("waiter held the monitor after wait: " + held);

        Object timedLock = new Object();
        Thread timed = Thread.ofVirtual().name("timed").start(() -> {
            synchronized (timedLock) {
                try {
                    timedLock.wait(60_000);
                } catch (InterruptedException e) {
                    // the end of the probe
                }
            }
        });
        awaitState(timed, Thread.State.TIMED_WAITING);
        System.out.println("timed waiter state: " + timed.getState());
        System.out.println("timed waiter toString: " + strip(timed.toString()));
        synchronized (timedLock) {
            timedLock.notify();
        }
        timed.join(10_000);
        System.out.println("timed waiter finished: " + !timed.isAlive());

        Object intLock = new Object();
        AtomicReference<String> seen = new AtomicReference<>("none");
        Thread interrupted = Thread.ofVirtual().name("interrupted").start(() -> {
            synchronized (intLock) {
                try {
                    intLock.wait();
                } catch (InterruptedException e) {
                    seen.set(e.getClass().getName() + " holdsLock=" + Thread.holdsLock(intLock)
                            + "|" + Thread.currentThread().isInterrupted());
                }
            }
        });
        awaitState(interrupted, Thread.State.WAITING);
        // Interrupt while this thread holds the monitor: the waiter must
        // re-acquire it (after this block) before it throws.
        synchronized (intLock) {
            interrupted.interrupt();
            Thread.sleep(50);
        }
        interrupted.join(10_000);
        String[] parts = seen.get().split("\\|");
        System.out.println("interrupted waiter: " + parts[0]);
        System.out.println("interrupted waiter interrupt status after catch: "
                + (parts.length > 1 ? parts[1] : "?"));
    }
}
