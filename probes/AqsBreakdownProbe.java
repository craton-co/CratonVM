import java.util.concurrent.CountDownLatch;
import java.util.concurrent.Semaphore;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Where does an AQS handoff's time actually go?
 *
 * `docs/known-issues/vm/aqs-thread-handoff-latency-20260803.md` measured
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

    private static long sink;

    private static void report(String label, long nanos, int n) {
        System.out.printf("%-46s %9.1f ns/op%n", label, nanos / (double) n);
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

        report("empty instance call (the scale)", emptyCall(p, ROUNDS), ROUNDS);
        System.out.println();
        report("ReentrantLock lock+unlock (uncontended)", lockUnlock(lock, ROUNDS), ROUNDS);
        report("ReentrantLock tryLock+unlock", tryLockUnlock(lock, ROUNDS), ROUNDS);
        report("ReentrantLock FAIR lock+unlock", lockUnlock(fair, ROUNDS), ROUNDS);
        report("Semaphore acquire+release (permits free)", semaphore(sem, ROUNDS), ROUNDS);
        report("CountDownLatch.await (already zero)", latchAwait(open, ROUNDS), ROUNDS);
        report("Condition.signal (no waiter)", signalNoWaiter(lock, cond, ROUNDS), ROUNDS);
        System.out.println();
        report("synchronized block (uncontended)", syncBlock(monitor, ROUNDS), ROUNDS);
        System.out.println();
        report("AtomicInteger.compareAndSet (tryAcquire CAS)", cas(ai, ROUNDS), ROUNDS);
        report("AtomicInteger.get", atomicGet(ai, ROUNDS), ROUNDS);
        report("Thread.onSpinWait", spinWait(ROUNDS), ROUNDS);

        if (sink == 42) {
            System.out.println("(unreachable)");
        }
    }
}
