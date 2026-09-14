package cratonvm;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.locks.Condition;
import java.util.concurrent.locks.ReentrantReadWriteLock;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Smoke test for the CRATONVM_REAL_AQS real-path gate.
 *
 * Run under CRATONVM_REAL_AQS=1 to verify that AbstractQueuedSynchronizer
 * (and its ReentrantLock and ReentrantReadWriteLock subclasses) runs real JDK
 * bytecode. The test acquires a lock on the main thread, observes it from a
 * second thread, releases it, and confirms both the locked and unlocked
 * states. It also invokes WriteLock.newCondition() through the real lock view.
 */
public class RealAqs {
    public static void main(String[] args) throws Exception {
        ReentrantLock lock = new ReentrantLock();
        CountDownLatch ready = new CountDownLatch(1);
        boolean[] sawLocked = {false};
        boolean[] sawUnlocked = {false};

        lock.lock();
        Thread t = new Thread(() -> {
            sawLocked[0] = lock.isLocked();
            ready.countDown();
            // Spin until we can acquire (after main releases).
            while (!lock.tryLock()) {
                Thread.yield();
            }
            sawUnlocked[0] = lock.isHeldByCurrentThread();
            lock.unlock();
        });
        t.start();

        // Wait for the background thread to observe the locked state.
        if (!ready.await(10, TimeUnit.SECONDS)) {
            throw new RuntimeException("timed out waiting for thread");
        }
        lock.unlock();
        t.join(10_000);

        ReentrantReadWriteLock readWriteLock = new ReentrantReadWriteLock();
        Condition writeCondition = readWriteLock.writeLock().newCondition();

        System.out.println("r:lock_seen_held=" + sawLocked[0]);
        System.out.println("r:thread_acquired=" + sawUnlocked[0]);
        System.out.println("r:now_unlocked=" + !lock.isLocked());
        System.out.println("r:rw_write_condition=" + (writeCondition != null));
        System.out.println("REAL_AQS_OK 4");
    }
}
