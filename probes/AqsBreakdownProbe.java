import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Semaphore;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Where does an AQS handoff's time actually go?
 *
 * `docs/internal/aqs-thread-handoff-latency-RETIRED-20260805.md` measured
 * `Condition.signal -> await` at 96.9 us against HotSpot's 7.4 us and blamed
 * `AbstractQueuedSynchronizer.acquire`'s pre-park spin. That was inference.
 * This probe measures the AQS operations that involve NO contention, NO
 * spinning and NO parking — one thread taking and releasing an uncontended
 * lock. Whatever that costs is a floor under every handoff.
 *
 * EVERY BENCHMARK IS AN INLINE LOOP IN ITS OWN METHOD, deliberately. The first
 * cut of this probe drove each operation through a `(int) -> void` lambda so
 * the harness could be a one-liner; on CratonVM the lambda's invokeinterface
 * costs ~2.2 us, which swamped every operation and made `AtomicInteger.get`
 * look like 3 us. A measurement harness whose abstraction costs more than the
 * thing measured reports the harness.
 *
 * ns/op, lower is better. Single-threaded throughout.
 */
public class AqsBreakdownProbe {

    private static final int WARMUP = 200_000;
    private static final int ROUNDS = 2_000_000;
    /**
     * Each rung is measured this many times and every pass is printed.
     *
     * A single warm-up-then-measure pass is not enough on this VM and the first
     * cut of this probe was wrong because of it: it reported an "empty instance
     * call" at 431 ns, which a 6-pass run shows converging to 8.6 ns by pass 2.
     * That inflated figure was then used to argue the AQS gap was "just 16
     * calls at the per-call floor" — it is not; `ReentrantLock` holds flat at
     * ~16 us across all six passes while an ordinary call is single-digit ns.
     * Read the LAST pass, and distrust any rung that has not gone flat.
     */
    private static final int PASSES = 4;

    private static long sink;

    private static void report(String label, long nanos, int n) {
        System.out.printf("%-46s %9.1f ns/op%n", label, nanos / (double) n);
    }

    /** A rung: run `n` iterations, return elapsed nanos. */
    private interface Rung {
        long run(int n);
    }

    /**
     * Run one rung `PASSES` times and print every pass on one line.
     *
     * The lambda here is NOT inside any timing loop — it is called once per
     * pass, and each implementation contains its own inline loop. That
     * distinction matters on this VM: an earlier version of this probe put the
     * lambda call *inside* the loop, where its ~2.2 us invokeinterface swamped
     * every measurement.
     */
    private static void pass(String label, Rung r) {
        System.out.printf("%-46s", label);
        for (int i = 0; i < PASSES; i++) {
            System.out.printf("%11.1f", r.run(ROUNDS) / (double) ROUNDS);
        }
        System.out.println();
    }

    private static long lockUnlock(ReentrantLock lock, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            lock.unlock();
        }
        return System.nanoTime() - t0;
    }

    private static long tryLockUnlock(ReentrantLock lock, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            if (lock.tryLock()) {
                lock.unlock();
            }
        }
        return System.nanoTime() - t0;
    }

    private static long semaphore(Semaphore sem, int n) throws InterruptedException {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sem.acquire();
            sem.release();
        }
        return System.nanoTime() - t0;
    }

    private static long latchAwait(CountDownLatch open, int n) throws InterruptedException {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            open.await();
        }
        return System.nanoTime() - t0;
    }

    private static long syncBlock(Object monitor, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            synchronized (monitor) {
                sink++;
            }
        }
        return System.nanoTime() - t0;
    }

    private static long cas(AtomicInteger ai, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            ai.compareAndSet(0, 0);
        }
        return System.nanoTime() - t0;
    }

    private static long atomicGet(AtomicInteger ai, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink += ai.get();
        }
        return System.nanoTime() - t0;
    }

    private static long spinWait(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            Thread.onSpinWait();
        }
        return System.nanoTime() - t0;
    }

    private static long signalNoWaiter(ReentrantLock lock, Condition cond, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            try {
                cond.signal();
            } finally {
                lock.unlock();
            }
        }
        return System.nanoTime() - t0;
    }

    /** A plain instance call, as the scale against which the rest is read. */
    private void empty() {
    }

    private static long emptyCall(AqsBreakdownProbe p, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            p.empty();
        }
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) throws Exception {
        AqsBreakdownProbe p = new AqsBreakdownProbe();
        ReentrantLock lock = new ReentrantLock();
        ReentrantLock fair = new ReentrantLock(true);
        Object monitor = new Object();
        AtomicInteger ai = new AtomicInteger();
        AtomicLong al = new AtomicLong();
        Semaphore sem = new Semaphore(Integer.MAX_VALUE / 2);
        CountDownLatch open = new CountDownLatch(0);
        Condition cond = lock.newCondition();

        // Warm every path.
        emptyCall(p, WARMUP);
        lockUnlock(lock, WARMUP);
        tryLockUnlock(lock, WARMUP);
        lockUnlock(fair, WARMUP);
        semaphore(sem, WARMUP);
        latchAwait(open, WARMUP);
        syncBlock(monitor, WARMUP);
        cas(ai, WARMUP);
        atomicGet(ai, WARMUP);
        spinWait(WARMUP);
        signalNoWaiter(lock, cond, WARMUP);
        if (al.get() == 42) {
            System.out.println("(unreachable)");
        }

        System.out.printf("%-46s", "rung");
        for (int i = 1; i <= PASSES; i++) {
            System.out.printf("%11d", i);
        }
        System.out.println("   (ns/op per pass; read the LAST)");

        pass("empty instance call (the scale)", n -> emptyCall(p, n));
        System.out.println();
        pass("ReentrantLock lock+unlock (uncontended)", n -> lockUnlock(lock, n));
        pass("ReentrantLock tryLock+unlock", n -> tryLockUnlock(lock, n));
        pass("ReentrantLock FAIR lock+unlock", n -> lockUnlock(fair, n));
        pass("Semaphore acquire+release (permits free)", n -> {
            try {
                return semaphore(sem, n);
            } catch (InterruptedException e) {
                throw new IllegalStateException(e);
            }
        });
        pass("CountDownLatch.await (already zero)", n -> {
            try {
                return latchAwait(open, n);
            } catch (InterruptedException e) {
                throw new IllegalStateException(e);
            }
        });
        pass("Condition.signal (no waiter)", n -> signalNoWaiter(lock, cond, n));
        System.out.println();
        pass("synchronized block (uncontended)", n -> syncBlock(monitor, n));
        System.out.println();
        pass("AtomicInteger.compareAndSet (tryAcquire CAS)", n -> cas(ai, n));
        pass("AtomicInteger.get", n -> atomicGet(ai, n));
        pass("Thread.onSpinWait", AqsBreakdownProbe::spinWait);

        if (sink == 42) {
            System.out.println("(unreachable)");
        }
    }
}
