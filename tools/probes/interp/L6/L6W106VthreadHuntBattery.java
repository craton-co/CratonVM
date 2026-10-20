// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L6 -- hunt battery for virtual-thread
// monitor, park and stack behaviours no earlier L6 probe pinned (suspected-OK
// guards: a line that differs from HotSpot 25 is a finding to page). Expected
// lines derived from the JDK 25 sources (`VirtualThread`, `LockSupport`,
// `Continuation.getStackTrace`) and HotSpot's `ObjectMonitor` semantics.
//
//  1. A virtual thread that waits with a REENTRANT hold (two nested
//     `synchronized` on one object) is WAITING, gets both levels back after
//     the notify (`holdsLock` at each level), and leaves the monitor free
//     (another thread enters at once).
//  2. A timed `Object.wait` is TIMED_WAITING; an interrupt throws
//     `InterruptedException` with the status cleared and the monitor held.
//  3. An unmounted parked virtual thread's `getStackTrace()`: the user frames
//     under `LockSupport.park`, the bottom frame `VirtualThread.run`, and no
//     hidden frame (`Continuation.enter0` / `yield0`, `Thread.runWith`, the
//     `VThreadContinuation` wrapper): `Continuation.getStackTrace` walks with
//     SHOW_REFLECT_FRAMES only. The same for the thread's own mounted trace.
//  4. `Thread.getAllStackTraces()` lists the caller, not a parked virtual
//     thread.
//  5. Permit edge values on a virtual thread: `parkNanos(0)`, `parkNanos(-1)`
//     and `parkNanos(Long.MIN_VALUE)` return without touching the permit
//     (LockSupport tests `nanos > 0`); `parkUntil(0)` CONSUMES it
//     (`VirtualThread.parkNanos` takes the permit before the `nanos > 0`
//     test); `parkNanos(Long.MAX_VALUE)` is TIMED_WAITING and an unpark ends
//     it.
//  6. `Thread.sleep` on a virtual thread with an interrupt pending throws an
//     `InterruptedException` with a null message (`VirtualThread.sleepNanos`;
//     a platform thread's says "sleep interrupted") and clears the status.
//  7. `notify` wakes one of two waiting virtual threads; `notifyAll` the rest.
//  8. InheritableThreadLocal: inherited (through `childValue`) by default,
//     not with `inheritInheritableThreadLocals(false)`; a plain ThreadLocal
//     is never inherited.
//
// Run: java -cp out L6W106VthreadHuntBattery (no flags; both modes, JIT and
// --nojit).
//
// HOTSPOT_EXPECTED_BEGIN
// reentrant wait state: WAITING
// reentrant wait holds: inner=true outer=true after=false
// monitor free after: true
// timed wait state: TIMED_WAITING
// timed wait interrupted: caught=true interruptedAfter=false holds=true
// parked trace: LockSupport.park <- Parker.parkHere <- Parker.run
// parked trace last: java.lang.VirtualThread.run
// parked trace hidden frames: 0
// own trace last: java.lang.VirtualThread.run
// own trace hidden frames: 0
// getAllStackTraces: caller=true parkedVirtual=false
// parkNanos(0/-1/MIN) kept permit: true
// parkUntil(0) consumed permit: true
// parkNanos(MAX) state: TIMED_WAITING
// parkNanos(MAX) unparked: true
// sleep with interrupt pending: threw=true msg=null interruptedAfter=false
// notify woke one: true other=WAITING
// notifyAll woke the rest: true
// itl default: parent-child
// itl not inherited: null
// plain thread local: null
// done
// HOTSPOT_EXPECTED_END

import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W106VthreadHuntBattery {
    static void awaitState(Thread t, Thread.State s) throws InterruptedException {
        long end = System.nanoTime() + 20_000_000_000L;
        while (t.getState() != s) {
            if (System.nanoTime() > end) {
                throw new IllegalStateException("timed out waiting for " + s + ", is " + t.getState());
            }
            Thread.sleep(1);
        }
    }

    static boolean hidden(StackTraceElement e) {
        String c = e.getClassName();
        return c.startsWith("jdk.internal.vm.Continuation")
                || (c.equals("java.lang.Thread") && e.getMethodName().equals("runWith"))
                || c.startsWith("java.lang.VirtualThread$VThreadContinuation");
    }

    static int hiddenCount(StackTraceElement[] trace) {
        int n = 0;
        for (StackTraceElement e : trace) {
            if (hidden(e)) {
                n++;
            }
        }
        return n;
    }

    static String last(StackTraceElement[] trace) {
        if (trace.length == 0) {
            return "<empty>";
        }
        StackTraceElement e = trace[trace.length - 1];
        return e.getClassName() + "." + e.getMethodName();
    }

    static void reentrantWait() throws Exception {
        Object lock = new Object();
        boolean[] r = new boolean[3];
        Thread t = Thread.ofVirtual().unstarted(() -> {
            synchronized (lock) {
                synchronized (lock) {
                    try {
                        lock.wait();
                    } catch (InterruptedException e) {
                        throw new RuntimeException(e);
                    }
                    r[0] = Thread.holdsLock(lock);
                }
                r[1] = Thread.holdsLock(lock);
            }
            r[2] = Thread.holdsLock(lock);
        });
        t.start();
        awaitState(t, Thread.State.WAITING);
        System.out.println("reentrant wait state: " + t.getState());
        synchronized (lock) {
            lock.notify();
        }
        t.join();
        System.out.println("reentrant wait holds: inner=" + r[0] + " outer=" + r[1] + " after=" + r[2]);
        Thread enter = Thread.ofPlatform().start(() -> {
            synchronized (lock) {
                lock.hashCode();
            }
        });
        enter.join(20_000);
        System.out.println("monitor free after: " + !enter.isAlive());
    }

    static void timedWaitInterrupt() throws Exception {
        Object lock = new Object();
        String[] out = new String[1];
        Thread t = Thread.ofVirtual().start(() -> {
            synchronized (lock) {
                try {
                    lock.wait(120_000);
                    out[0] = "caught=false";
                } catch (InterruptedException e) {
                    out[0] = "caught=true interruptedAfter=" + Thread.currentThread().isInterrupted()
                            + " holds=" + Thread.holdsLock(lock);
                }
            }
        });
        awaitState(t, Thread.State.TIMED_WAITING);
        System.out.println("timed wait state: " + t.getState());
        t.interrupt();
        t.join();
        System.out.println("timed wait interrupted: " + out[0]);
    }

    static final class Parker implements Runnable {
        volatile boolean go;
        volatile StackTraceElement[] own;

        @Override
        public void run() {
            own = Thread.currentThread().getStackTrace();
            parkHere();
        }

        void parkHere() {
            while (!go) {
                LockSupport.park();
            }
        }
    }

    static void parkedTrace() throws Exception {
        Parker p = new Parker();
        Thread t = Thread.ofVirtual().start(p);
        awaitState(t, Thread.State.WAITING);
        StackTraceElement[] trace = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : trace) {
            String c = e.getClassName();
            String simple;
            if (c.equals("java.util.concurrent.locks.LockSupport")) {
                simple = "LockSupport";
            } else if (c.startsWith(L6W106VthreadHuntBattery.class.getName())) {
                simple = c.substring(c.lastIndexOf('$') + 1);
            } else {
                continue;
            }
            if (sb.length() > 0) {
                sb.append(" <- ");
            }
            sb.append(simple).append('.').append(e.getMethodName());
        }
        System.out.println("parked trace: " + sb);
        System.out.println("parked trace last: " + last(trace));
        System.out.println("parked trace hidden frames: " + hiddenCount(trace));
        System.out.println("own trace last: " + last(p.own));
        System.out.println("own trace hidden frames: " + hiddenCount(p.own));
        boolean caller = Thread.getAllStackTraces().containsKey(Thread.currentThread());
        boolean parked = Thread.getAllStackTraces().containsKey(t);
        System.out.println("getAllStackTraces: caller=" + caller + " parkedVirtual=" + parked);
        p.go = true;
        LockSupport.unpark(t);
        t.join();
    }

    static void permitEdges() throws Exception {
        String[] out = new String[2];
        Thread t = Thread.ofVirtual().start(() -> {
            Thread me = Thread.currentThread();
            LockSupport.unpark(me);
            LockSupport.parkNanos(0);
            LockSupport.parkNanos(-1);
            LockSupport.parkNanos(Long.MIN_VALUE);
            long t0 = System.nanoTime();
            LockSupport.parkNanos(10_000_000_000L);
            out[0] = String.valueOf(System.nanoTime() - t0 < 5_000_000_000L);
            LockSupport.unpark(me);
            LockSupport.parkUntil(0);
            t0 = System.nanoTime();
            LockSupport.parkNanos(400_000_000L);
            out[1] = String.valueOf(System.nanoTime() - t0 >= 300_000_000L);
        });
        t.join();
        System.out.println("parkNanos(0/-1/MIN) kept permit: " + out[0]);
        System.out.println("parkUntil(0) consumed permit: " + out[1]);

        boolean[] done = new boolean[1];
        Thread m = Thread.ofVirtual().start(() -> {
            LockSupport.parkNanos(Long.MAX_VALUE);
            done[0] = true;
        });
        awaitState(m, Thread.State.TIMED_WAITING);
        System.out.println("parkNanos(MAX) state: " + m.getState());
        LockSupport.unpark(m);
        m.join(20_000);
        System.out.println("parkNanos(MAX) unparked: " + (done[0] && !m.isAlive()));
    }

    static void sleepInterruptPending() throws Exception {
        String[] out = new String[1];
        Thread t = Thread.ofVirtual().start(() -> {
            Thread.currentThread().interrupt();
            try {
                Thread.sleep(60_000);
                out[0] = "threw=false";
            } catch (InterruptedException e) {
                out[0] = "threw=true msg=" + e.getMessage() + " interruptedAfter="
                        + Thread.currentThread().isInterrupted();
            }
        });
        t.join();
        System.out.println("sleep with interrupt pending: " + out[0]);
    }

    static void notifyOneThenAll() throws Exception {
        Object lock = new Object();
        AtomicInteger woke = new AtomicInteger();
        Runnable waiter = () -> {
            synchronized (lock) {
                try {
                    lock.wait();
                } catch (InterruptedException e) {
                    throw new RuntimeException(e);
                }
            }
            woke.incrementAndGet();
        };
        Thread a = Thread.ofVirtual().start(waiter);
        Thread b = Thread.ofVirtual().start(waiter);
        awaitState(a, Thread.State.WAITING);
        awaitState(b, Thread.State.WAITING);
        synchronized (lock) {
            lock.notify();
        }
        long end = System.nanoTime() + 20_000_000_000L;
        while (woke.get() == 0 && System.nanoTime() < end) {
            Thread.sleep(1);
        }
        Thread.sleep(200);
        Thread other = a.isAlive() ? a : b;
        System.out.println("notify woke one: " + (woke.get() == 1) + " other=" + other.getState());
        synchronized (lock) {
            lock.notifyAll();
        }
        a.join(20_000);
        b.join(20_000);
        System.out.println("notifyAll woke the rest: " + (woke.get() == 2));
    }

    static final InheritableThreadLocal<String> ITL = new InheritableThreadLocal<>() {
        @Override
        protected String childValue(String parent) {
            return parent + "-child";
        }
    };
    static final ThreadLocal<String> TL = new ThreadLocal<>();

    static void inheritance() throws Exception {
        ITL.set("parent");
        TL.set("parent");
        String[] out = new String[3];
        Thread a = Thread.ofVirtual().start(() -> {
            out[0] = ITL.get();
            out[2] = TL.get();
        });
        a.join();
        Thread b = Thread.ofVirtual().inheritInheritableThreadLocals(false).start(() -> out[1] = ITL.get());
        b.join();
        System.out.println("itl default: " + out[0]);
        System.out.println("itl not inherited: " + out[1]);
        System.out.println("plain thread local: " + out[2]);
    }

    public static void main(String[] args) throws Exception {
        reentrantWait();
        timedWaitInterrupt();
        parkedTrace();
        permitEdges();
        sleepInterruptPending();
        notifyOneThenAll();
        inheritance();
        System.out.println("done");
    }
}
