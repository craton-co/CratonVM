// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L1 -- bench for
// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST` (default OFF): with it, over a
// real JDK (either mode) `ReentrantLock.lock()` / `unlock()` are two reviewed
// `Intrinsic`s serving the uncontended and reentrant shapes on the JDK's own
// fields (one native call each) instead of the JDK's 17-invocation call tree
// (three of whose calls cross the native funnel). Every other shape re-enters
// the JDK body; `CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL=1` (beside the
// switch) forces that fallback on every call, which prices it.
//
// A/B, one binary, interleaved, >= 3 rounds, medians of the per-run minima,
// in each of `--jdk-only` (default) and `--compatible`, JIT and `--nojit`:
//   cratonvm L1W77ReentrantLockBench                                                          (off)
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST=1 cratonvm L1W77ReentrantLockBench                (on)
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST=1 CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL=1 \
//     cratonvm L1W77ReentrantLockBench                                                          (fallback)
//   java L1W77ReentrantLockBench                                                               (HotSpot)
//
// Rows (stdout, min ns/op over REPS=5 rounds):
//   lock-unlock      lock(); unlock() on a non-fair lock, ns per pair (the served shape);
//   reentrant        lock(); lock(); unlock(); unlock(), ns per outer pair;
//   fair             lock(); unlock() on a fair lock, ns per pair;
//   trylock-unlock   tryLock(); unlock() -- tryLock is never served (control for unlock);
//   cond-signal      lock(); signal() with no waiter; unlock() (ConditionObject reads the owner);
//   idle-queue       lock(); unlock() on a lock whose queue head was set by a past
//                    hand-off (the JDK never clears it);
//   pbq-peek         PriorityBlockingQueue.peek() (one lock pair around a field read);
//   pbq-size         PriorityBlockingQueue.size();
//   handoff          two threads, 20 000 lock(); count++; unlock() each on one lock, ns per
//                    pair (contended: declines, parks and wakes; noisy -- a smoke row for
//                    the fallback under contention, not a gate);
//   await-1us        lock(); awaitNanos(1000); unlock(), ns per op (fullyRelease and the
//                    re-acquire are JDK bytecode around the served pair).
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum lock-unlock=200000 reentrant=200000 fair=200000 trylock-unlock=200000 cond-signal=100000 idle-queue=200000 pbq-peek=20000000 pbq-size=3200000 handoff=40000 await-1us=2000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.PriorityBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;

public class L1W77ReentrantLockBench {
    static final int REPS = 5;
    static final int N = 200_000;
    static final int COND_N = 100_000;
    static final int HANDOFF_EACH = 20_000;
    static final int AWAIT_N = 2_000;

    static long lockUnlock(ReentrantLock lock) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            lock.lock();
            try {
                sum++;
            } finally {
                lock.unlock();
            }
        }
        return sum;
    }

    static long reentrant(ReentrantLock lock) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            lock.lock();
            lock.lock();
            sum += lock.getHoldCount() - 1;
            lock.unlock();
            lock.unlock();
        }
        return sum;
    }

    static long tryLockUnlock(ReentrantLock lock) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            if (lock.tryLock()) {
                sum++;
                lock.unlock();
            }
        }
        return sum;
    }

    static long condSignal(ReentrantLock lock, Condition c) {
        long sum = 0;
        for (int i = 0; i < COND_N; i++) {
            lock.lock();
            try {
                c.signal();
                sum++;
            } finally {
                lock.unlock();
            }
        }
        return sum;
    }

    static long pbqPeek(PriorityBlockingQueue<Integer> q) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += q.peek();
        }
        return sum;
    }

    static long pbqSize(PriorityBlockingQueue<Integer> q) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += q.size();
        }
        return sum;
    }

    static long handoff() throws InterruptedException {
        ReentrantLock lock = new ReentrantLock();
        long[] count = new long[1];
        Runnable r = () -> {
            for (int i = 0; i < HANDOFF_EACH; i++) {
                lock.lock();
                try {
                    count[0]++;
                } finally {
                    lock.unlock();
                }
            }
        };
        Thread a = new Thread(r, "rl-bench-a");
        Thread b = new Thread(r, "rl-bench-b");
        a.start();
        b.start();
        a.join();
        b.join();
        return count[0];
    }

    static long await1us(ReentrantLock lock, Condition c) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < AWAIT_N; i++) {
            lock.lock();
            try {
                c.awaitNanos(1000);
                sum += lock.getHoldCount();
            } finally {
                lock.unlock();
            }
        }
        return sum;
    }

    /** A lock whose AQS `head` is set: one contended hand-off, then idle. */
    static ReentrantLock onceContended() throws InterruptedException {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        Thread t = new Thread(() -> {
            lock.lock();
            lock.unlock();
        }, "rl-bench-once");
        t.start();
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(20);
        while (!lock.hasQueuedThread(t) && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        lock.unlock();
        t.join();
        return lock;
    }

    public static void main(String[] args) throws Exception {
        ReentrantLock nonfair = new ReentrantLock();
        ReentrantLock fair = new ReentrantLock(true);
        ReentrantLock condLock = new ReentrantLock();
        Condition cond = condLock.newCondition();
        ReentrantLock idle = onceContended();
        PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
        for (int v = 115; v >= 100; v--) {
            q.offer(v);
        }
        String[] names = {"lock-unlock", "reentrant", "fair", "trylock-unlock", "cond-signal",
            "idle-queue", "pbq-peek", "pbq-size", "handoff", "await-1us"};
        long[] per = {N, N, N, N, COND_N, N, N, N, 2L * HANDOFF_EACH, AWAIT_N};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        for (int r = 0; r < REPS; r++) {
            long[] t = new long[names.length + 1];
            t[0] = System.nanoTime();
            sums[0] = lockUnlock(nonfair);
            t[1] = System.nanoTime();
            sums[1] = reentrant(nonfair);
            t[2] = System.nanoTime();
            sums[2] = lockUnlock(fair);
            t[3] = System.nanoTime();
            sums[3] = tryLockUnlock(nonfair);
            t[4] = System.nanoTime();
            sums[4] = condSignal(condLock, cond);
            t[5] = System.nanoTime();
            sums[5] = lockUnlock(idle);
            t[6] = System.nanoTime();
            sums[6] = pbqPeek(q);
            t[7] = System.nanoTime();
            sums[7] = pbqSize(q);
            t[8] = System.nanoTime();
            sums[8] = handoff();
            t[9] = System.nanoTime();
            sums[9] = await1us(condLock, cond);
            t[10] = System.nanoTime();
            for (int i = 0; i < names.length; i++) {
                best[i] = Math.min(best[i], t[i + 1] - t[i]);
            }
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-15s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
        }
        StringBuilder sb = new StringBuilder("checksum");
        for (int i = 0; i < names.length; i++) {
            sb.append(' ').append(names[i]).append('=').append(sums[i]);
        }
        System.out.println(sb);
        System.out.println("done");
    }
}
