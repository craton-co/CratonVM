// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L1:
//
// * `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH` (the `JdkCountDownLatch`
//   HotSpot-alignment row; docs/internal/fixed-bugs/interpreter-i70-L1-proposal-a-countdownlatch-family-row-DONE-20261104.md):
//   `--compatible` over a real JDK no longer registers the eleven
//   `CountDownLatch` stand-in registrations, so the JDK 25 bodies run (AQS
//   `Sync` state, `LockSupport.park(this)` with the `Sync` as the blocker), as
//   `--jdk-only` already did.
//
// Columns, read without an agent: the state; `jmx-same` (JMX agrees);
// `lock=` the class of `ThreadInfo.getLockName()`; `blocker=` the class of
// `LockSupport.getBlocker(t)`; `lock-is-blocker` (the JMX lock name is the
// blocker's class@identity-hash).
//
// Run in both modes: `cratonvm L1W71CountDownLatchBodies` (jdk-only) and
// `cratonvm --compatible L1W71CountDownLatchBodies`; both must print the block
// below. `=0` arm: `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH=0` brings the
// stand-ins back; what moves then is the stand-ins' difference
// (expected from code reading: `blocker=null`, `lock-is-blocker=false`, the
// stack, both pending-interrupt rows, and the negative-timeout row, filed as
// i71-L1 pages).
//
// The expected block is DERIVED from the JDK 25 sources (CountDownLatch /
// AbstractQueuedSynchronizer / LockSupport), not yet captured from the host
// oracle: refresh it from the host's HotSpot run before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// new(3): getCount=3 toString=[Count = 3]
// countDown x2: getCount=1 toString=[Count = 1]
// countDown x2 more: getCount=0 toString=[Count = 0]
// new(-1): java.lang.IllegalArgumentException message=count < 0
// await(0 ms) count=1: false
// await(1, null): java.lang.NullPointerException message=Cannot invoke "java.util.concurrent.TimeUnit.toNanos(long)" because "unit" is null
// pending interrupt, await() count=0: java.lang.InterruptedException interrupted-after=false
// pending interrupt, await(1 s) count=0: java.lang.InterruptedException interrupted-after=false
// pending interrupt, await(0 s) count=1: java.lang.InterruptedException interrupted-after=false
// await() (count=1): WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch$Sync blocker=java.util.concurrent.CountDownLatch$Sync lock-is-blocker=true
// await(60 s) (count=1): TIMED_WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch$Sync blocker=java.util.concurrent.CountDownLatch$Sync lock-is-blocker=true
// synchronized latch.wait(): WAITING jmx-same=true lock=java.util.concurrent.CountDownLatch blocker=null lock-is-blocker=false
// stack(await): jdk.internal.misc.Unsafe.park > java.util.concurrent.locks.LockSupport.park > java.util.concurrent.locks.AbstractQueuedSynchronizer.acquire > java.util.concurrent.locks.AbstractQueuedSynchronizer.acquireSharedInterruptibly > java.util.concurrent.CountDownLatch.await
// await() interrupted while parked: java.lang.InterruptedException
// await(60 s) interrupted while parked: java.lang.InterruptedException
// eight waiters released: 8 of 8
// virtual await(): WAITING blocker=java.util.concurrent.CountDownLatch$Sync released=true
// stress: releases=2000 count=0
// await(-1 s) count=1: false
// await(-1 s) count=0: true
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L1W71CountDownLatchBodies {
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    interface Blocking {
        void run() throws Exception;
    }

    static boolean waiting(Thread.State s) {
        return s == Thread.State.WAITING || s == Thread.State.TIMED_WAITING;
    }

    static String className(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String lockClass(String name) {
        if (name == null) {
            return "null";
        }
        int at = name.indexOf('@');
        return at < 0 ? name : name.substring(0, at);
    }

    static String suffix(Object o) {
        String s = String.valueOf(o);
        int at = s.indexOf('[');
        return at < 0 ? s : s.substring(at);
    }

    static String exceptionOf(Blocking b) {
        try {
            b.run();
            return "no exception";
        } catch (Exception e) {
            return e.getClass().getName() + " message=" + e.getMessage();
        }
    }

    static Thread start(String label, Blocking body) {
        Thread t = new Thread(() -> {
            try {
                body.run();
            } catch (InterruptedException e) {
                // Interrupted at the end of the row.
            } catch (Exception e) {
                System.out.println(label + ": threw " + e);
            }
        }, label);
        t.setDaemon(true);
        t.start();
        return t;
    }

    static void settle(Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!waiting(t.getState()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
    }

    static ThreadInfo reading(Thread t) throws InterruptedException {
        ThreadInfo info = MX.getThreadInfo(t.threadId());
        for (int i = 0; i < 50; i++) {
            Thread.State s = t.getState();
            if (waiting(s) && info != null && info.getThreadState() == s && info.getLockName() != null) {
                break;
            }
            Thread.sleep(2);
            info = MX.getThreadInfo(t.threadId());
        }
        return info;
    }

    /** Prints one row for a thread blocked by {@code body}, then runs {@code release} and joins it. */
    static void row(String label, Blocking body, Runnable release) throws Exception {
        Thread t = start(label, body);
        settle(t);
        ThreadInfo info = reading(t);
        Thread.State s = t.getState();
        Object blocker = LockSupport.getBlocker(t);
        boolean same = info != null && info.getThreadState() == s;
        String lockName = info == null ? null : info.getLockName();
        boolean lockIsBlocker = blocker != null && lockName != null
                && lockName.equals(blocker.getClass().getName() + '@'
                        + Integer.toHexString(System.identityHashCode(blocker)));
        System.out.println(label + ": " + s + " jmx-same=" + same
                + " lock=" + lockClass(lockName)
                + " blocker=" + className(blocker)
                + " lock-is-blocker=" + lockIsBlocker);
        release.run();
        t.join(5000);
        if (t.isAlive()) {
            System.out.println(label + ": still alive after release");
        }
    }

    interface Answering {
        boolean get() throws Exception;
    }

    static String boundedAnswer(Answering body) throws InterruptedException {
        String[] answer = {"no answer after 2 s"};
        Thread t = new Thread(() -> {
            try {
                answer[0] = String.valueOf(body.get());
            } catch (Exception e) {
                answer[0] = e.getClass().getName();
            }
        }, "bounded");
        t.setDaemon(true);
        t.start();
        t.join(2000);
        return answer[0];
    }

    static String interruptedRow(Blocking body) {
        Thread.currentThread().interrupt();
        String outcome;
        try {
            body.run();
            outcome = "returned";
        } catch (Exception e) {
            outcome = e.getClass().getName();
        }
        // Thread.interrupted() clears what is left, so the next row starts clean.
        return outcome + " interrupted-after=" + Thread.interrupted();
    }

    static String interruptWhileParked(String label, Blocking body) throws InterruptedException {
        String[] outcome = {"no outcome"};
        Thread t = new Thread(() -> {
            try {
                body.run();
                outcome[0] = "returned";
            } catch (Exception e) {
                outcome[0] = e.getClass().getName();
            }
        }, label);
        t.setDaemon(true);
        t.start();
        settle(t);
        t.interrupt();
        t.join(5000);
        return outcome[0];
    }

    public static void main(String[] args) throws Exception {
        // ---- Single-threaded answers ----
        CountDownLatch l = new CountDownLatch(3);
        System.out.println("new(3): getCount=" + l.getCount() + " toString=" + suffix(l));
        l.countDown();
        l.countDown();
        System.out.println("countDown x2: getCount=" + l.getCount() + " toString=" + suffix(l));
        l.countDown();
        l.countDown();
        System.out.println("countDown x2 more: getCount=" + l.getCount() + " toString=" + suffix(l));
        System.out.println("new(-1): " + exceptionOf(() -> new CountDownLatch(-1)));
        CountDownLatch one = new CountDownLatch(1);
        CountDownLatch zero = new CountDownLatch(0);
        System.out.println("await(0 ms) count=1: " + one.await(0, TimeUnit.MILLISECONDS));
        System.out.println("await(1, null): " + exceptionOf(() -> one.await(1, null)));
        System.out.println("pending interrupt, await() count=0: " + interruptedRow(zero::await));
        System.out.println("pending interrupt, await(1 s) count=0: "
                + interruptedRow(() -> zero.await(1, TimeUnit.SECONDS)));
        // Interpreter round i3 wave 79, lane L1: the interrupt test comes before
        // `nanosTimeout <= 0L` too (`tryAcquireSharedNanos`).
        System.out.println("pending interrupt, await(0 s) count=1: "
                + interruptedRow(() -> one.await(0, TimeUnit.SECONDS)));

        // ---- Parked waiters: state, JMX lock, park blocker ----
        CountDownLatch a = new CountDownLatch(1);
        row("await() (count=1)", a::await, a::countDown);

        CountDownLatch b = new CountDownLatch(1);
        row("await(60 s) (count=1)", () -> b.await(60, TimeUnit.SECONDS), b::countDown);

        // The program's own monitor wait on a latch object: HotSpot names the
        // latch, not a `Sync` (the stand-in's JMX rewrite must not apply).
        CountDownLatch m = new CountDownLatch(1);
        boolean[] notified = {false};
        row("synchronized latch.wait()", () -> {
            synchronized (m) {
                while (!notified[0]) {
                    m.wait();
                }
            }
        }, () -> {
            synchronized (m) {
                notified[0] = true;
                m.notifyAll();
            }
        });

        // The top five frames of a parked `await()`.
        CountDownLatch s = new CountDownLatch(1);
        Thread stacked = start("stacked", s::await);
        settle(stacked);
        StackTraceElement[] st = stacked.getStackTrace();
        StringBuilder top = new StringBuilder();
        for (int i = 0; i < Math.min(5, st.length); i++) {
            if (i > 0) {
                top.append(" > ");
            }
            top.append(st[i].getClassName()).append('.').append(st[i].getMethodName());
        }
        System.out.println("stack(await): " + top);
        s.countDown();
        stacked.join(5000);

        // An interrupt wakes a parked waiter with InterruptedException.
        CountDownLatch i1 = new CountDownLatch(1);
        System.out.println("await() interrupted while parked: " + interruptWhileParked("int-await", i1::await));
        System.out.println("await(60 s) interrupted while parked: "
                + interruptWhileParked("int-timed", () -> i1.await(60, TimeUnit.SECONDS)));

        // One countDown releases every waiter.
        CountDownLatch gate = new CountDownLatch(1);
        AtomicInteger released = new AtomicInteger();
        Thread[] waiters = new Thread[8];
        for (int i = 0; i < waiters.length; i++) {
            waiters[i] = start("waiter-" + i, () -> {
                gate.await();
                released.incrementAndGet();
            });
        }
        for (Thread w : waiters) {
            settle(w);
        }
        gate.countDown();
        for (Thread w : waiters) {
            w.join(5000);
        }
        System.out.println("eight waiters released: " + released.get() + " of " + waiters.length);

        // A virtual thread parked in `await()`.
        CountDownLatch v = new CountDownLatch(1);
        boolean[] vdone = {false};
        Thread vt = Thread.ofVirtual().name("virtual-await").start(() -> {
            try {
                v.await();
                vdone[0] = true;
            } catch (InterruptedException e) {
                // not expected
            }
        });
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5);
        while (!waiting(vt.getState()) && System.nanoTime() < deadline) {
            Thread.sleep(5);
        }
        Thread.sleep(50);
        Thread.State vs = vt.getState();
        Object vb = LockSupport.getBlocker(vt);
        v.countDown();
        vt.join(5000);
        System.out.println("virtual await(): " + vs + " blocker=" + className(vb) + " released=" + vdone[0]);

        // ---- Handoff stress with allocation ----
        int rounds = 2000;
        CountDownLatch[] ping = new CountDownLatch[rounds];
        for (int i = 0; i < rounds; i++) {
            ping[i] = new CountDownLatch(2);
        }
        AtomicInteger releases = new AtomicInteger();
        Thread[] workers = new Thread[2];
        for (int w = 0; w < workers.length; w++) {
            workers[w] = new Thread(() -> {
                Object sink = null;
                for (int i = 0; i < rounds; i++) {
                    sink = new byte[128];
                    ping[i].countDown();
                    try {
                        ping[i].await();
                    } catch (InterruptedException e) {
                        return;
                    }
                }
                if (sink != null) {
                    releases.addAndGet(rounds / workers.length);
                }
            }, "stress-" + w);
            workers[w].start();
        }
        for (Thread w : workers) {
            w.join();
        }
        System.out.println("stress: releases=" + releases.get() + " count=" + ping[rounds - 1].getCount());

        // On a daemon thread with a bounded join: the stand-in casts a negative
        // millisecond timeout to `u64`, so its deadline is centuries away and
        // the call parks for good where HotSpot answers `false` at once
        // (i71-L1-countdownlatch-stand-in-negative-timeout-waits-forever).
        System.out.println("await(-1 s) count=1: " + boundedAnswer(() -> one.await(-1, TimeUnit.SECONDS)));
        System.out.println("await(-1 s) count=0: " + boundedAnswer(() -> zero.await(-1, TimeUnit.SECONDS)));
        System.out.println("done");
    }
}
