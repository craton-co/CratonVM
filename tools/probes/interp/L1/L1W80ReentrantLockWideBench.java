// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 80, lane L1 -- bench for
// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE` (default OFF; meaningful with
// `CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST`, default on, registered only in a
// VM without a JIT unless `CRATONVM_INTRINSIC_BODY_COMPILES=1`): with it,
// `ReentrantLock.lockInterruptibly()` and `tryLock()` are served like `lock()`
// / `unlock()`. `lockInterruptibly()` is the acquire half of every blocking
// queue's `take` / `put` (LinkedBlockingQueue, ArrayBlockingQueue, ...), so a
// ThreadPoolExecutor hand-off pays it once per task.
//
// A/B, one binary, interleaved, >= 5 rounds, medians of the per-run minima;
// each arm in `--jdk-only` (default), `--jdk-only --nojit` and `--compatible`
// (and `--compatible --nojit`):
//   cratonvm L1W80ReentrantLockWideBench                                                  (off)
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE=1 cratonvm L1W80ReentrantLockWideBench  (wide)
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE=1 CRATONVM_REENTRANT_LOCK_FAST_DECLINE_ALL=1 \
//     cratonvm L1W80ReentrantLockWideBench                                                (price of a decline)
//   CRATONVM_REENTRANT_LOCK_UNCONTENDED_FAST_WIDE=1 CRATONVM_INTRINSIC_BODY_COMPILES=1 \
//     cratonvm L1W80ReentrantLockWideBench                                                (JIT VM: rows registered, bodies compile)
//   java L1W80ReentrantLockWideBench                                                       (HotSpot)
// One row only: `L1W80ReentrantLockWideBench <row>`.
//
// Rows (stdout, min ns/op over REPS=5 rounds):
//   lock-unlock       lock(); unlock() -- control (served by the wave-77 rows in both arms);
//   li-unlock         lockInterruptibly(); unlock(), ns per pair;
//   li-reentrant      lockInterruptibly() twice, unlock() twice, ns per outer pair;
//   trylock-unlock    tryLock(); unlock(), ns per pair;
//   trylock-held      tryLock() on a lock another (parked) thread holds: false, ns per call;
//   fair-trylock      tryLock(); unlock() on a fair lock;
//   lbq-offer-take    LinkedBlockingQueue offer(); take() (take: lockInterruptibly), single thread;
//   lbq-put-poll      LinkedBlockingQueue put(); poll(1, SECONDS) (both lockInterruptibly);
//   abq-offer-take    ArrayBlockingQueue offer(); take();
//   executor-rt       single-thread executor submit(i).get() round trip (park / unpark dominated).
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum lock-unlock=200000 li-unlock=200000 li-reentrant=200000 trylock-unlock=200000 trylock-held=0 fair-trylock=200000 lbq-offer-take=19999900000 lbq-put-poll=19999900000 abq-offer-take=19999900000 executor-rt=1999000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.ReentrantLock;

public class L1W80ReentrantLockWideBench {
    static final int REPS = 5;
    static final int N = 200_000;
    static final int EXEC_N = 2_000;

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

    static long liUnlock(ReentrantLock lock) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            lock.lockInterruptibly();
            try {
                sum++;
            } finally {
                lock.unlock();
            }
        }
        return sum;
    }

    static long liReentrant(ReentrantLock lock) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            lock.lockInterruptibly();
            lock.lockInterruptibly();
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

    static long tryLockHeld(ReentrantLock held) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            if (held.tryLock()) {
                sum++;
                held.unlock();
            }
        }
        return sum;
    }

    static long lbqOfferTake(LinkedBlockingQueue<Integer> q) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            q.offer(i);
            sum += q.take();
        }
        return sum;
    }

    static long lbqPutPoll(LinkedBlockingQueue<Integer> q) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            q.put(i);
            sum += q.poll(1, TimeUnit.SECONDS);
        }
        return sum;
    }

    static long abqOfferTake(ArrayBlockingQueue<Integer> q) throws InterruptedException {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            q.offer(i);
            sum += q.take();
        }
        return sum;
    }

    static long executorRoundTrip(ExecutorService ex) throws Exception {
        long sum = 0;
        for (int i = 0; i < EXEC_N; i++) {
            final int v = i;
            sum += ex.submit(() -> v).get();
        }
        return sum;
    }

    /** A thread that takes `lock` and parks until `release` is set. */
    static Thread holder(ReentrantLock lock, Object gate, boolean[] flags) throws InterruptedException {
        Thread t = new Thread(() -> {
            lock.lock();
            try {
                synchronized (gate) {
                    flags[0] = true;
                    gate.notifyAll();
                    while (!flags[1]) {
                        try {
                            gate.wait();
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                }
            } finally {
                lock.unlock();
            }
        }, "rl-wide-holder");
        t.setDaemon(true);
        t.start();
        synchronized (gate) {
            while (!flags[0]) {
                gate.wait();
            }
        }
        return t;
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        ReentrantLock nonfair = new ReentrantLock();
        ReentrantLock fair = new ReentrantLock(true);
        ReentrantLock held = new ReentrantLock();
        Object gate = new Object();
        boolean[] flags = new boolean[2];
        Thread holderThread = holder(held, gate, flags);
        LinkedBlockingQueue<Integer> lbq = new LinkedBlockingQueue<>();
        ArrayBlockingQueue<Integer> abq = new ArrayBlockingQueue<>(16);
        ExecutorService ex = Executors.newSingleThreadExecutor();
        String[] names = {"lock-unlock", "li-unlock", "li-reentrant", "trylock-unlock", "trylock-held",
            "fair-trylock", "lbq-offer-take", "lbq-put-poll", "abq-offer-take", "executor-rt"};
        long[] per = {N, N, N, N, N, N, N, N, N, EXEC_N};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        boolean[] ran = new boolean[names.length];
        for (int r = 0; r < REPS; r++) {
            for (int i = 0; i < names.length; i++) {
                if (only != null && !only.equals(names[i])) {
                    continue;
                }
                long t0 = System.nanoTime();
                long s;
                switch (i) {
                    case 0: s = lockUnlock(nonfair); break;
                    case 1: s = liUnlock(nonfair); break;
                    case 2: s = liReentrant(nonfair); break;
                    case 3: s = tryLockUnlock(nonfair); break;
                    case 4: s = tryLockHeld(held); break;
                    case 5: s = tryLockUnlock(fair); break;
                    case 6: s = lbqOfferTake(lbq); break;
                    case 7: s = lbqPutPoll(lbq); break;
                    case 8: s = abqOfferTake(abq); break;
                    default: s = executorRoundTrip(ex); break;
                }
                long dt = System.nanoTime() - t0;
                best[i] = Math.min(best[i], dt);
                sums[i] = s;
                ran[i] = true;
            }
        }
        synchronized (gate) {
            flags[1] = true;
            gate.notifyAll();
        }
        holderThread.join();
        ex.shutdown();
        ex.awaitTermination(20, TimeUnit.SECONDS);
        StringBuilder sb = new StringBuilder("checksum");
        for (int i = 0; i < names.length; i++) {
            if (!ran[i]) {
                continue;
            }
            System.out.printf("%-15s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
            sb.append(' ').append(names[i]).append('=').append(sums[i]);
        }
        System.out.println(sb);
        System.out.println("done");
    }
}
