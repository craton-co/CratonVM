// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 99, lane L6 -- page
// docs/internal/fixed-bugs/interpreter-i94-L6-thread-death-sweeps-every-inflated-monitor-FIXED-20261015.md:
// a PLATFORM thread's death skips the walk of every inflated monitor when it
// holds none it entered outside structured locking (JNI MonitorEnter, a VM
// native's monitor_enter) and its Java ended by a return or a throw
// (CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT, default ON; HotSpot's
// JavaThread::exit skips release_monitors_owned_by_thread when its
// _jni_monitor_count is 0). Every monitor a dead platform thread touched must
// be free afterwards: platform threads lock and return, throw out of
// synchronized blocks and methods (uncaught), wait (timed, notified,
// interrupted -- caught inside the block and escaping it), are contended
// while holding, and inflate a monitor each in bulk; then a fresh platform
// thread and the main thread lock every one of those monitors. No JNI.
//
// Run (both modes, JIT and --nojit, the switch both ways; with
// CRATONVM_DBG_VTHREAD_CENSUS=1 the at-exit rows platform_death_sweeps_skipped
// (> 0) and platform_death_sweep_skip_misses / death_sweep_skip_misses (0)):
//   cratonvm L6W99PlatformThreadDeathMonitors
//   CRATONVM_VTHREAD_DEATH_SWEEP_BY_COUNT=0 cratonvm L6W99PlatformThreadDeathMonitors
//   cratonvm --compatible L6W99PlatformThreadDeathMonitors
//
// HOTSPOT_EXPECTED_BEGIN
// sync-return: done=true
// sync-throw: uncaught=java.lang.IllegalStateException:block
// sync-method-throw: uncaught=java.lang.IllegalStateException:method
// nested-throw: uncaught=java.lang.IllegalArgumentException:nested
// wait-timed: done=true
// wait-notified: notified=true
// wait-interrupted-caught: interrupted=true holdsAfter=true
// wait-interrupted-escapes: uncaught=java.lang.RuntimeException:java.lang.InterruptedException
// contended-holder: contender=true
// handoff: second=true
// bulk: threads=200 waited=200
// checker: ok locked=211
// main: locked=211
// done
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicBoolean;

public class L6W99PlatformThreadDeathMonitors {
    static final Map<String, String> UNCAUGHT = new ConcurrentHashMap<>();
    static final List<Object> LOCKS = new ArrayList<>();

    static Object lock() {
        Object o = new Object();
        LOCKS.add(o);
        return o;
    }

    static Thread start(String name, Runnable r) {
        Thread t = new Thread(r, name);
        t.setUncaughtExceptionHandler((th, e) ->
            UNCAUGHT.put(th.getName(), e.getClass().getName() + ":" + e.getMessage()));
        t.start();
        return t;
    }

    static void waitForState(Thread t, Thread.State s) throws InterruptedException {
        while (t.getState() != s) {
            Thread.sleep(1);
        }
    }

    static final class SyncThrower {
        synchronized void boom() {
            throw new IllegalStateException("method");
        }
    }

    public static void main(String[] args) throws Exception {
        // sync-return
        Object a = lock();
        AtomicBoolean done = new AtomicBoolean();
        Thread t = start("sync-return", () -> {
            synchronized (a) {
                done.set(true);
            }
        });
        t.join();
        System.out.println("sync-return: done=" + done.get());

        // sync-throw: an uncaught exception out of a synchronized block.
        Object b = lock();
        t = start("sync-throw", () -> {
            synchronized (b) {
                throw new IllegalStateException("block");
            }
        });
        t.join();
        System.out.println("sync-throw: uncaught=" + UNCAUGHT.get("sync-throw"));

        // sync-method-throw: out of a synchronized method.
        SyncThrower st = new SyncThrower();
        LOCKS.add(st);
        t = start("sync-method-throw", st::boom);
        t.join();
        System.out.println("sync-method-throw: uncaught=" + UNCAUGHT.get("sync-method-throw"));

        // nested-throw: two block monitors, the inner one inflated by a wait.
        Object c1 = lock();
        Object c2 = lock();
        t = start("nested-throw", () -> {
            synchronized (c1) {
                synchronized (c2) {
                    try {
                        c2.wait(1);
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                    throw new IllegalArgumentException("nested");
                }
            }
        });
        t.join();
        System.out.println("nested-throw: uncaught=" + UNCAUGHT.get("nested-throw"));

        // wait-timed
        Object d = lock();
        AtomicBoolean waited = new AtomicBoolean();
        t = start("wait-timed", () -> {
            synchronized (d) {
                try {
                    d.wait(2);
                } catch (InterruptedException e) {
                    return;
                }
                waited.set(true);
            }
        });
        t.join();
        System.out.println("wait-timed: done=" + waited.get());

        // wait-notified: main notifies once the thread waits.
        Object e = lock();
        AtomicBoolean notified = new AtomicBoolean();
        AtomicBoolean go = new AtomicBoolean();
        t = start("wait-notified", () -> {
            synchronized (e) {
                while (!go.get()) {
                    try {
                        e.wait();
                    } catch (InterruptedException ex) {
                        return;
                    }
                }
                notified.set(true);
            }
        });
        waitForState(t, Thread.State.WAITING);
        synchronized (e) {
            go.set(true);
            e.notifyAll();
        }
        t.join();
        System.out.println("wait-notified: notified=" + notified.get());

        // wait-interrupted-caught: the interrupt is caught inside the block.
        Object f = lock();
        AtomicBoolean interrupted = new AtomicBoolean();
        AtomicBoolean holdsAfter = new AtomicBoolean();
        t = start("wait-interrupted-caught", () -> {
            synchronized (f) {
                try {
                    f.wait();
                } catch (InterruptedException ex) {
                    interrupted.set(true);
                    holdsAfter.set(Thread.holdsLock(f));
                }
            }
        });
        waitForState(t, Thread.State.WAITING);
        t.interrupt();
        t.join();
        System.out.println("wait-interrupted-caught: interrupted=" + interrupted.get()
            + " holdsAfter=" + holdsAfter.get());

        // wait-interrupted-escapes: the interrupt leaves the block as an
        // uncaught exception.
        Object g = lock();
        t = start("wait-interrupted-escapes", () -> {
            synchronized (g) {
                try {
                    g.wait();
                } catch (InterruptedException ex) {
                    throw new RuntimeException(ex);
                }
            }
        });
        waitForState(t, Thread.State.WAITING);
        t.interrupt();
        t.join();
        System.out.println("wait-interrupted-escapes: uncaught="
            + UNCAUGHT.get("wait-interrupted-escapes"));

        // contended-holder: the dying thread held a monitor main contended
        // (inflated by the contention), released it, and died.
        Object h = lock();
        CountDownLatch holding = new CountDownLatch(1);
        AtomicBoolean release = new AtomicBoolean();
        t = start("contended-holder", () -> {
            synchronized (h) {
                holding.countDown();
                while (!release.get()) {
                    Thread.onSpinWait();
                }
            }
        });
        holding.await();
        Thread releaser = new Thread(() -> {
            try {
                Thread.sleep(20);
            } catch (InterruptedException ex) {
                // not interrupted
            }
            release.set(true);
        });
        releaser.start();
        boolean contender;
        synchronized (h) {
            contender = true;
        }
        t.join();
        releaser.join();
        System.out.println("contended-holder: contender=" + contender);

        // handoff: the first thread holds; the second blocks on it, gets it
        // after the first's release, and both die.
        Object k = lock();
        CountDownLatch firstHolds = new CountDownLatch(1);
        AtomicBoolean firstGo = new AtomicBoolean();
        AtomicBoolean second = new AtomicBoolean();
        Thread first = start("handoff-1", () -> {
            synchronized (k) {
                firstHolds.countDown();
                while (!firstGo.get()) {
                    Thread.onSpinWait();
                }
            }
        });
        firstHolds.await();
        Thread secondT = start("handoff-2", () -> {
            synchronized (k) {
                second.set(true);
            }
        });
        waitForState(secondT, Thread.State.BLOCKED);
        firstGo.set(true);
        first.join();
        secondT.join();
        System.out.println("handoff: second=" + second.get());

        // bulk: 200 platform threads, each inflating its own monitor with a
        // timed wait, then dying.
        List<Thread> bulk = new ArrayList<>();
        int[] waitedCount = new int[1];
        Object countLock = new Object();
        for (int i = 0; i < 200; i++) {
            Object m = lock();
            bulk.add(start("bulk-" + i, () -> {
                synchronized (m) {
                    try {
                        m.wait(1);
                    } catch (InterruptedException ex) {
                        return;
                    }
                }
                synchronized (countLock) {
                    waitedCount[0]++;
                }
            }));
        }
        for (Thread x : bulk) {
            x.join();
        }
        System.out.println("bulk: threads=" + bulk.size() + " waited=" + waitedCount[0]);

        // Every monitor is free: a fresh platform thread locks them all
        // (bounded, so a leak prints instead of hanging), then main does.
        int[] got = new int[1];
        Thread checker = new Thread(() -> {
            for (Object o : LOCKS) {
                synchronized (o) {
                    got[0]++;
                }
            }
        }, "checker");
        checker.setDaemon(true);
        checker.start();
        checker.join(10_000);
        if (checker.isAlive()) {
            System.out.println("checker: HANG locked=" + got[0] + " of " + LOCKS.size());
            System.exit(1);
        }
        System.out.println("checker: ok locked=" + got[0]);
        int mine = 0;
        for (Object o : LOCKS) {
            synchronized (o) {
                mine++;
            }
        }
        System.out.println("main: locked=" + mine);
        System.out.println("done");
    }
}
