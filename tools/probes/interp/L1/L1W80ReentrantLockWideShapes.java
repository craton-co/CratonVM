// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 80, lane L1 -- behavioural probe for
// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE` (default OFF; meaningful with
// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST`, default on in a VM without a
// JIT): with both, `ReentrantLock.lockInterruptibly()` and `tryLock()` are two
// more reviewed `Intrinsic`s on the JDK's own fields. An interrupted caller of
// `lockInterruptibly()` and every unserved shape re-enter the JDK bodies, so
// the exception, its frames and the cleared status must be HotSpot's.
//
// Run, every arm must print the block below (JIT and `--nojit`, both modes):
//   cratonvm L1W80ReentrantLockWideShapes                                        (wide off)
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE=1 cratonvm L1W80ReentrantLockWideShapes
//   cratonvm --compatible L1W80ReentrantLockWideShapes
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE=1 cratonvm --compatible L1W80ReentrantLockWideShapes
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE=1 CRATONVM_INTRINSIC_BODY_COMPILES=1 cratonvm L1W80ReentrantLockWideShapes
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE=1 CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL=1 cratonvm L1W80ReentrantLockWideShapes
//
// The expected block is DERIVED from the JDK 25 sources (ReentrantLock,
// AbstractQueuedSynchronizer, Thread.interrupted), not captured from the host
// oracle: refresh it from the host's HotSpot run before reading a difference.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// li-basic: hold=1 string=[Locked by thread main] reentrantHold=2 after=0 string=[Unlocked]
// li-interrupted: java.lang.InterruptedException message=null statusAfter=false locked=false
// li-interrupted frames: java.util.concurrent.locks.ReentrantLock$Sync.lockInterruptibly < java.util.concurrent.locks.ReentrantLock.lockInterruptibly < L1W80ReentrantLockWideShapes.li
// li-interrupted-reentrant: java.lang.InterruptedException hold=1 statusAfter=false
// li-waiter: queued=true state=WAITING
// li-waiter frames: java.util.concurrent.locks.ReentrantLock$Sync.lockInterruptibly < java.util.concurrent.locks.ReentrantLock.lockInterruptibly < L1W80ReentrantLockWideShapes$LiWaiter.run
// li-waiter: outcome=java.lang.InterruptedException statusAfter=false got=false
// li-waiter exception frames: java.util.concurrent.locks.ReentrantLock$Sync.lockInterruptibly < java.util.concurrent.locks.ReentrantLock.lockInterruptibly < L1W80ReentrantLockWideShapes$LiWaiter.run
// li-waiter: afterUnlock=[Unlocked] relock=true hold=1
// li-handoff: got=true hold=1 after=[Unlocked]
// trylock: first=true hold=1 second=true hold=2 after=0 string=[Unlocked]
// trylock-other-owner: tryLock=false string=[Locked by thread rl-owner] stillHeld=true
// trylock-interrupted: tryLock=true statusKept=true hold=1
// trylock-fair: fair=true tryLock=true hold=1 queued=false
// jmx: tryLock=[java.util.concurrent.locks.ReentrantLock$NonfairSync] lockInterruptibly=[java.util.concurrent.locks.ReentrantLock$FairSync] released=[]
// lbq: offer=true take=1 put-take=2 poll=3 timedPoll=null size=0
// lbq-waiter: took=42 size=0
// stress: interruptibly=40000 tryLock=40000 locked=false
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.ReentrantLock;

public class L1W80ReentrantLockWideShapes {

    static String state(ReentrantLock lock) {
        String s = lock.toString();
        return s.substring(s.indexOf('['));
    }

    static String frames(StackTraceElement[] st, int max) {
        StringBuilder sb = new StringBuilder();
        int n = 0;
        for (StackTraceElement e : st) {
            if (n == max) {
                break;
            }
            if (n > 0) {
                sb.append(" < ");
            }
            sb.append(e.getClassName()).append('.').append(e.getMethodName());
            n++;
        }
        return sb.toString();
    }

    /** The frames of the lock's own classes and this probe's, in order. */
    static String lockFrames(StackTraceElement[] st) {
        StringBuilder sb = new StringBuilder();
        for (StackTraceElement e : st) {
            String c = e.getClassName();
            if (c.startsWith("java.util.concurrent.locks.ReentrantLock")
                    || c.startsWith("L1W80ReentrantLockWideShapes")) {
                if (sb.length() > 0) {
                    sb.append(" < ");
                }
                sb.append(c).append('.').append(e.getMethodName());
            }
        }
        return sb.toString();
    }

    static void li(ReentrantLock lock) throws InterruptedException {
        lock.lockInterruptibly();
    }

    static void liBasic() throws InterruptedException {
        ReentrantLock lock = new ReentrantLock();
        lock.lockInterruptibly();
        int hold = lock.getHoldCount();
        String s = state(lock);
        lock.lockInterruptibly();
        int reentrantHold = lock.getHoldCount();
        lock.unlock();
        lock.unlock();
        System.out.println("li-basic: hold=" + hold + " string=" + s + " reentrantHold=" + reentrantHold
                + " after=" + lock.getHoldCount() + " string=" + state(lock));
    }

    static void liInterrupted() {
        ReentrantLock lock = new ReentrantLock();
        Thread.currentThread().interrupt();
        try {
            li(lock);
            System.out.println("li-interrupted: acquired");
            lock.unlock();
        } catch (InterruptedException e) {
            System.out.println("li-interrupted: " + e.getClass().getName() + " message=" + e.getMessage()
                    + " statusAfter=" + Thread.currentThread().isInterrupted() + " locked=" + lock.isLocked());
            System.out.println("li-interrupted frames: " + frames(e.getStackTrace(), 3));
        }
        Thread.interrupted();
    }

    /** The interrupt check comes before the reentrant arm of initialTryLock. */
    static void liInterruptedReentrant() {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        Thread.currentThread().interrupt();
        String out;
        try {
            lock.lockInterruptibly();
            out = "acquired";
            lock.unlock();
        } catch (InterruptedException e) {
            out = e.getClass().getName();
        }
        int hold = lock.getHoldCount();
        boolean status = Thread.interrupted();
        lock.unlock();
        System.out.println("li-interrupted-reentrant: " + out + " hold=" + hold + " statusAfter=" + status);
    }

    static class LiWaiter implements Runnable {
        final ReentrantLock lock;
        volatile boolean got;
        volatile String outcome = "none";
        volatile boolean statusAfter;
        volatile String exceptionFrames = "";

        LiWaiter(ReentrantLock lock) {
            this.lock = lock;
        }

        @Override
        public void run() {
            try {
                lock.lockInterruptibly();
                try {
                    got = true;
                    outcome = "acquired";
                } finally {
                    lock.unlock();
                }
            } catch (InterruptedException e) {
                outcome = e.getClass().getName();
                exceptionFrames = lockFrames(e.getStackTrace());
            }
            statusAfter = Thread.currentThread().isInterrupted();
        }
    }

    static void waitParked(ReentrantLock lock, Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
        while (!lock.hasQueuedThread(t) || t.getState() != Thread.State.WAITING) {
            if (System.nanoTime() > deadline) {
                throw new IllegalStateException("waiter never parked");
            }
            Thread.sleep(1);
        }
    }

    static void liWaiter() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        LiWaiter w = new LiWaiter(lock);
        Thread t = new Thread(w, "rl-li-waiter");
        lock.lock();
        t.start();
        waitParked(lock, t);
        System.out.println("li-waiter: queued=" + lock.hasQueuedThread(t) + " state=" + t.getState());
        System.out.println("li-waiter frames: " + lockFrames(t.getStackTrace()));
        t.interrupt();
        t.join();
        System.out.println("li-waiter: outcome=" + w.outcome + " statusAfter=" + w.statusAfter
                + " got=" + w.got);
        System.out.println("li-waiter exception frames: " + w.exceptionFrames);
        lock.unlock();
        boolean relock = lock.tryLock();
        int hold = lock.getHoldCount();
        String after = "[Unlocked]";
        if (relock) {
            lock.unlock();
            after = state(lock);
        }
        System.out.println("li-waiter: afterUnlock=" + after + " relock=" + relock + " hold=" + hold);

        // A waiter that is handed the lock (no interrupt).
        LiWaiter h = new LiWaiter(lock);
        Thread ht = new Thread(h, "rl-li-handoff");
        lock.lock();
        ht.start();
        waitParked(lock, ht);
        lock.unlock();
        ht.join();
        System.out.println("li-handoff: got=" + h.got + " hold=" + (h.got ? 1 : 0) + " after=" + state(lock));
    }

    static void tryLockRows() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        boolean first = lock.tryLock();
        int hold1 = lock.getHoldCount();
        boolean second = lock.tryLock();
        int hold2 = lock.getHoldCount();
        lock.unlock();
        lock.unlock();
        System.out.println("trylock: first=" + first + " hold=" + hold1 + " second=" + second + " hold="
                + hold2 + " after=" + lock.getHoldCount() + " string=" + state(lock));

        ReentrantLock held = new ReentrantLock();
        Object gate = new Object();
        boolean[] go = new boolean[2];
        Thread owner = new Thread(() -> {
            held.lock();
            try {
                synchronized (gate) {
                    go[0] = true;
                    gate.notifyAll();
                    while (!go[1]) {
                        try {
                            gate.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                }
            } finally {
                held.unlock();
            }
        }, "rl-owner");
        owner.start();
        synchronized (gate) {
            while (!go[0]) {
                gate.wait();
            }
        }
        boolean t = held.tryLock();
        String s = state(held);
        boolean stillHeld = held.isLocked() && !held.isHeldByCurrentThread();
        synchronized (gate) {
            go[1] = true;
            gate.notifyAll();
        }
        owner.join();
        System.out.println("trylock-other-owner: tryLock=" + t + " string=" + s + " stillHeld=" + stillHeld);

        ReentrantLock li = new ReentrantLock();
        Thread.currentThread().interrupt();
        boolean ti = li.tryLock();
        boolean kept = Thread.interrupted();
        int holdI = li.getHoldCount();
        if (ti) {
            li.unlock();
        }
        System.out.println("trylock-interrupted: tryLock=" + ti + " statusKept=" + kept + " hold=" + holdI);

        ReentrantLock fair = new ReentrantLock(true);
        boolean tf = fair.tryLock();
        int holdF = fair.getHoldCount();
        boolean queued = fair.hasQueuedThreads();
        if (tf) {
            fair.unlock();
        }
        System.out.println("trylock-fair: fair=" + fair.isFair() + " tryLock=" + tf + " hold=" + holdF
                + " queued=" + queued);
    }

    static List<String> lockedSynchronizers() {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ThreadInfo[] info = mx.getThreadInfo(new long[] {Thread.currentThread().threadId()}, false, true);
        List<String> out = new ArrayList<>();
        for (LockInfo li : info[0].getLockedSynchronizers()) {
            out.add(li.getClassName());
        }
        return out;
    }

    static void jmx() throws InterruptedException {
        ReentrantLock nonfair = new ReentrantLock();
        nonfair.tryLock();
        List<String> a = lockedSynchronizers();
        nonfair.unlock();
        ReentrantLock fair = new ReentrantLock(true);
        fair.lockInterruptibly();
        List<String> b = lockedSynchronizers();
        fair.unlock();
        List<String> c = lockedSynchronizers();
        System.out.println("jmx: tryLock=" + a + " lockInterruptibly=" + b + " released=" + c);
    }

    static void lbq() throws Exception {
        LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>();
        boolean offer = q.offer(1);
        int take = q.take();
        q.put(2);
        int putTake = q.take();
        q.offer(3, 1, TimeUnit.SECONDS);
        Integer poll = q.poll(1, TimeUnit.SECONDS);
        Integer timed = q.poll(1, TimeUnit.MILLISECONDS);
        System.out.println("lbq: offer=" + offer + " take=" + take + " put-take=" + putTake + " poll=" + poll
                + " timedPoll=" + timed + " size=" + q.size());

        LinkedBlockingQueue<Integer> w = new LinkedBlockingQueue<>();
        int[] took = new int[1];
        Thread consumer = new Thread(() -> {
            try {
                took[0] = w.take();
            } catch (InterruptedException e) {
                took[0] = -1;
            }
        }, "rl-lbq-consumer");
        consumer.start();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
        while (consumer.getState() != Thread.State.WAITING) {
            if (System.nanoTime() > deadline) {
                throw new IllegalStateException("consumer never waited");
            }
            Thread.sleep(1);
        }
        w.put(42);
        consumer.join();
        System.out.println("lbq-waiter: took=" + took[0] + " size=" + w.size());
    }

    static void stress() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        int[] count = new int[2];
        Runnable li = () -> {
            for (int i = 0; i < 20_000; i++) {
                try {
                    lock.lockInterruptibly();
                } catch (InterruptedException e) {
                    return;
                }
                try {
                    count[0]++;
                } finally {
                    lock.unlock();
                }
            }
        };
        Runnable tl = () -> {
            for (int i = 0; i < 20_000; i++) {
                while (!lock.tryLock()) {
                    Thread.onSpinWait();
                }
                try {
                    count[1]++;
                } finally {
                    lock.unlock();
                }
            }
        };
        Thread a = new Thread(li, "rl-stress-a");
        Thread b = new Thread(li, "rl-stress-b");
        a.start();
        b.start();
        a.join();
        b.join();
        Thread c = new Thread(tl, "rl-stress-c");
        Thread d = new Thread(tl, "rl-stress-d");
        c.start();
        d.start();
        c.join();
        d.join();
        System.out.println("stress: interruptibly=" + count[0] + " tryLock=" + count[1] + " locked="
                + lock.isLocked());
    }

    public static void main(String[] args) throws Exception {
        liBasic();
        liInterrupted();
        liInterruptedReentrant();
        liWaiter();
        tryLockRows();
        jmx();
        lbq();
        stress();
        System.out.println("done");
    }
}
