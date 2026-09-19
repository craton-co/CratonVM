import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.Semaphore;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.AbstractQueuedSynchronizer;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantLock;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.StampedLock;

/**
 * JDK-only corpus: AQS -- reentrant locks, read/write locks, conditions,
 * timeout, interrupt.
 *
 * {@code AbstractQueuedSynchronizer} is pure Java built on one intrinsic
 * (compare-and-set) plus park/unpark. Under {@code --jdk-only} the whole
 * hierarchy must run as real bytecode with real field state -- a native shim
 * that fakes hold counts or queue membership shows up here immediately.
 *
 * Determinism: every rendezvous is a latch or a barrier; no sleeps are used to
 * order anything that is asserted, and no thread names or ids are printed.
 */
public class RJdkAqs {
    static final long T = 30;
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void reentrantLock() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        check(!lock.isLocked() && !lock.isHeldByCurrentThread(), "initial state");
        lock.lock();
        try {
            check(lock.isLocked() && lock.isHeldByCurrentThread(), "locked state");
            check(lock.getHoldCount() == 1, "hold count 1");
            lock.lock();
            check(lock.getHoldCount() == 2, "reentrant hold count 2");
            check(lock.tryLock(), "tryLock by the owner must succeed (reentrant)");
            check(lock.getHoldCount() == 3, "hold count 3");
            lock.unlock();
            lock.unlock();
            check(lock.getHoldCount() == 1, "hold count back to 1");
        } finally {
            lock.unlock();
        }
        check(!lock.isLocked() && lock.getHoldCount() == 0, "fully released");

        // Unlocking a lock you do not own is an error.
        boolean threw = false;
        try {
            lock.unlock();
        } catch (IllegalMonitorStateException expected) {
            threw = true;
        }
        check(threw, "unlock without holding must throw IllegalMonitorStateException");

        // A second thread must NOT be able to take a held lock.
        lock.lock();
        AtomicBoolean got = new AtomicBoolean(true);
        AtomicBoolean timedOut = new AtomicBoolean(false);
        CountDownLatch done = new CountDownLatch(1);
        Thread other = new Thread(() -> {
            got.set(lock.tryLock());
            try {
                timedOut.set(!lock.tryLock(50, TimeUnit.MILLISECONDS));
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
            done.countDown();
        });
        other.start();
        check(done.await(T, TimeUnit.SECONDS), "contender never finished");
        other.join();
        check(!got.get(), "tryLock from another thread must fail while held");
        check(timedOut.get(), "timed tryLock must time out while held");
        check(lock.isLocked() && lock.isHeldByCurrentThread(), "still held by us");
        lock.unlock();

        // Mutual exclusion under contention: the counter must be exact.
        final ReentrantLock counterLock = new ReentrantLock();
        final int[] counter = new int[1];
        int threads = 6;
        int perThread = 5000;
        CyclicBarrier start = new CyclicBarrier(threads);
        List<Thread> ts = new ArrayList<>();
        for (int i = 0; i < threads; i++) {
            Thread t = new Thread(() -> {
                try {
                    start.await();
                } catch (Exception e) {
                    throw new AssertionError(e);
                }
                for (int j = 0; j < perThread; j++) {
                    counterLock.lock();
                    try {
                        counter[0]++;
                    } finally {
                        counterLock.unlock();
                    }
                }
            });
            ts.add(t);
            t.start();
        }
        for (Thread t : ts) {
            t.join(T * 1000);
        }
        check(counter[0] == threads * perThread, "contended counter: " + counter[0]);
        System.out.println("CK RJdkAqs reentrant counter=" + counter[0]);
    }

    static void lockInterruptibly() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        AtomicBoolean interrupted = new AtomicBoolean(false);
        CountDownLatch waiting = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(1);
        Thread t = new Thread(() -> {
            waiting.countDown();
            try {
                lock.lockInterruptibly();
                lock.unlock();
            } catch (InterruptedException e) {
                interrupted.set(true);
            }
            done.countDown();
        });
        t.start();
        check(waiting.await(T, TimeUnit.SECONDS), "waiter never started");
        // Spin until the waiter is actually queued on the lock, then interrupt.
        long deadline = System.nanoTime() + T * 1_000_000_000L;
        while (!lock.hasQueuedThreads() && System.nanoTime() < deadline) {
            Thread.yield();
        }
        check(lock.hasQueuedThreads(), "the contender must be queued on the lock");
        check(lock.getQueueLength() == 1, "queue length: " + lock.getQueueLength());
        check(lock.hasQueuedThread(t), "hasQueuedThread");
        t.interrupt();
        check(done.await(T, TimeUnit.SECONDS), "waiter never finished");
        t.join();
        check(interrupted.get(), "lockInterruptibly must throw InterruptedException on interrupt");
        lock.unlock();
        System.out.println("CK RJdkAqs lockInterruptibly=ok");
    }

    static void conditions() throws Exception {
        ReentrantLock lock = new ReentrantLock();
        Condition notEmpty = lock.newCondition();
        Condition notFull = lock.newCondition();
        final List<Integer> queue = new ArrayList<>();
        final int capacity = 4;
        final int items = 200;
        final List<Integer> consumed = Collections.synchronizedList(new ArrayList<>());

        Thread producer = new Thread(() -> {
            for (int i = 0; i < items; i++) {
                lock.lock();
                try {
                    while (queue.size() == capacity) {
                        notFull.await();
                    }
                    queue.add(i);
                    notEmpty.signal();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                } finally {
                    lock.unlock();
                }
            }
        });
        Thread consumer = new Thread(() -> {
            for (int i = 0; i < items; i++) {
                lock.lock();
                try {
                    while (queue.isEmpty()) {
                        notEmpty.await();
                    }
                    consumed.add(queue.remove(0));
                    notFull.signal();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                } finally {
                    lock.unlock();
                }
            }
        });
        producer.start();
        consumer.start();
        producer.join(T * 1000);
        consumer.join(T * 1000);
        check(consumed.size() == items, "consumed count: " + consumed.size());
        boolean fifo = true;
        for (int i = 0; i < items; i++) {
            fifo &= consumed.get(i) == i;
        }
        check(fifo, "a single-producer/single-consumer bounded queue must stay FIFO");
        check(queue.isEmpty(), "queue drained");

        // await() on a condition you do not own is an error.
        boolean threw = false;
        try {
            notEmpty.signal();
        } catch (IllegalMonitorStateException expected) {
            threw = true;
        }
        check(threw, "signal without the lock must throw IllegalMonitorStateException");

        // Timed await must return false on timeout and must reacquire the lock.
        lock.lock();
        try {
            check(!notEmpty.await(50, TimeUnit.MILLISECONDS), "await must time out");
            check(lock.isHeldByCurrentThread(), "await must reacquire the lock on timeout");
        } finally {
            lock.unlock();
        }

        // awaitUninterruptibly swallows the interrupt but leaves the flag set.
        lock.lock();
        CountDownLatch inWait = new CountDownLatch(1);
        AtomicBoolean flagSet = new AtomicBoolean(false);
        Thread waiter = new Thread(() -> {
            lock.lock();
            try {
                inWait.countDown();
                notEmpty.awaitUninterruptibly();
                flagSet.set(Thread.currentThread().isInterrupted());
            } finally {
                lock.unlock();
            }
        });
        waiter.setDaemon(true);
        waiter.start();
        lock.unlock();
        check(inWait.await(T, TimeUnit.SECONDS), "waiter never entered");
        // getWaitQueueLength/hasWaiters require the lock, so the spin has to
        // take it on every probe.
        long deadline = System.nanoTime() + T * 1_000_000_000L;
        int waiters = 0;
        while (System.nanoTime() < deadline) {
            lock.lock();
            try {
                waiters = lock.getWaitQueueLength(notEmpty);
            } finally {
                lock.unlock();
            }
            if (waiters > 0) {
                break;
            }
            Thread.yield();
        }
        lock.lock();
        try {
            check(waiters == 1, "condition wait queue: " + waiters);
            check(lock.hasWaiters(notEmpty), "hasWaiters");
        } finally {
            lock.unlock();
        }
        waiter.interrupt();
        lock.lock();
        try {
            notEmpty.signalAll();
        } finally {
            lock.unlock();
        }
        waiter.join(T * 1000);
        check(flagSet.get(), "awaitUninterruptibly must leave the interrupt flag set");
        System.out.println("CK RJdkAqs conditions consumed=" + consumed.size() + " fifo=" + fifo);
    }

    static void readWriteLock() throws Exception {
        ReentrantReadWriteLock rw = new ReentrantReadWriteLock();
        rw.readLock().lock();
        rw.readLock().lock();
        check(rw.getReadHoldCount() == 2, "read hold count");
        check(rw.getReadLockCount() == 2, "read lock count");
        check(!rw.isWriteLocked(), "not write locked");
        // A writer cannot enter while readers hold the lock.
        check(!rw.writeLock().tryLock(), "write tryLock must fail while read-held");
        rw.readLock().unlock();
        rw.readLock().unlock();
        check(rw.getReadHoldCount() == 0, "read released");

        rw.writeLock().lock();
        check(rw.isWriteLocked() && rw.isWriteLockedByCurrentThread(), "write locked");
        check(rw.getWriteHoldCount() == 1, "write hold count");
        // Downgrade: acquire read while holding write, then drop write.
        rw.readLock().lock();
        rw.writeLock().unlock();
        check(!rw.isWriteLocked() && rw.getReadHoldCount() == 1, "write->read downgrade");
        rw.readLock().unlock();

        // Concurrent readers really do proceed together.
        int readers = 4;
        CyclicBarrier inside = new CyclicBarrier(readers);
        CountDownLatch done = new CountDownLatch(readers);
        AtomicInteger together = new AtomicInteger();
        for (int i = 0; i < readers; i++) {
            new Thread(() -> {
                rw.readLock().lock();
                try {
                    inside.await(T, TimeUnit.SECONDS);
                    together.incrementAndGet();
                } catch (Exception e) {
                    throw new AssertionError(e);
                } finally {
                    rw.readLock().unlock();
                    done.countDown();
                }
            }).start();
        }
        check(done.await(T, TimeUnit.SECONDS), "readers must be able to hold the lock together");
        check(together.get() == readers, "concurrent readers: " + together.get());

        // StampedLock: optimistic read, validate, and conversion.
        StampedLock sl = new StampedLock();
        long stamp = sl.tryOptimisticRead();
        check(stamp != 0, "optimistic read stamp");
        check(sl.validate(stamp), "optimistic read must validate with no writer");
        long w = sl.writeLock();
        check(!sl.validate(stamp), "an optimistic stamp must not validate after a write");
        check(sl.isWriteLocked(), "StampedLock write held");
        sl.unlockWrite(w);
        long r = sl.readLock();
        check(sl.isReadLocked(), "StampedLock read held");
        long converted = sl.tryConvertToWriteLock(r);
        check(converted != 0, "read->write conversion with a single reader must succeed");
        sl.unlockWrite(converted);
        check(!sl.isWriteLocked() && !sl.isReadLocked(), "StampedLock fully released");
        System.out.println("CK RJdkAqs rwlock readers=" + together.get());
    }

    /** A custom AQS subclass: the whole point of the class being extensible. */
    static final class Mutex extends AbstractQueuedSynchronizer {
        private static final long serialVersionUID = 1L;

        @Override
        protected boolean tryAcquire(int arg) {
            return compareAndSetState(0, 1);
        }

        @Override
        protected boolean tryRelease(int arg) {
            setState(0);
            return true;
        }

        @Override
        protected boolean isHeldExclusively() {
            return getState() == 1;
        }

        void lock() {
            acquire(1);
        }

        boolean tryLock() {
            return tryAcquire(1);
        }

        void unlock() {
            release(1);
        }
    }

    static void customSynchronizer() throws Exception {
        Mutex m = new Mutex();
        check(m.tryLock(), "custom AQS tryLock");
        check(m.isHeldExclusively(), "custom AQS held");
        check(!m.tryLock(), "custom AQS is not reentrant");
        AtomicBoolean second = new AtomicBoolean(true);
        Thread t = new Thread(() -> second.set(m.tryLock()));
        t.start();
        t.join(T * 1000);
        check(!second.get(), "another thread must not acquire a held custom mutex");
        m.unlock();
        check(!m.isHeldExclusively(), "custom AQS released");
        m.lock();
        m.unlock();

        // Semaphore and CountDownLatch are AQS users too.
        Semaphore sem = new Semaphore(2);
        check(sem.tryAcquire(2), "semaphore acquire 2");
        check(!sem.tryAcquire(), "semaphore exhausted");
        check(sem.availablePermits() == 0, "permits");
        sem.release(2);
        check(sem.availablePermits() == 2, "permits restored");
        CountDownLatch latch = new CountDownLatch(2);
        check(latch.getCount() == 2, "latch count");
        latch.countDown();
        check(!latch.await(20, TimeUnit.MILLISECONDS), "latch must not open early");
        latch.countDown();
        check(latch.await(T, TimeUnit.SECONDS), "latch must open at zero");
        check(latch.getCount() == 0, "latch drained");
        System.out.println("CK RJdkAqs custom=ok permits=" + sem.availablePermits());
    }

    public static void main(String[] args) throws Exception {
        reentrantLock();
        lockInterruptibly();
        conditions();
        readWriteLock();
        customSynchronizer();
        List<String> covered = new ArrayList<>(Arrays.asList(
                "ReentrantLock", "ReentrantReadWriteLock", "StampedLock",
                "Condition", "Semaphore", "CountDownLatch", "AbstractQueuedSynchronizer"));
        Collections.sort(covered);
        System.out.println("CK RJdkAqs covered=" + covered);
        System.out.println("CK RJdkAqs checks=" + checks);
        System.out.println("PASS RJdkAqs (" + checks + " checks)");
    }
}
