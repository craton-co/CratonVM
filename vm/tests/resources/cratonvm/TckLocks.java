package cratonvm;
import java.util.concurrent.locks.*;
public class TckLocks {
    public static int reentrant_lock_unlock() {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        try { return lock.isLocked() ? 1 : 0; }
        finally { lock.unlock(); }
    }
    public static int reentrant_held() {
        ReentrantLock lock = new ReentrantLock();
        lock.lock();
        try { return lock.isHeldByCurrentThread() ? 1 : 0; }
        finally { lock.unlock(); }
    }
    public static int reentrant_tryLock() {
        ReentrantLock lock = new ReentrantLock();
        return lock.tryLock() ? 1 : 0;
    }
    public static int reentrant_reentrant() {
        ReentrantLock lock = new ReentrantLock();
        lock.lock(); lock.lock();
        int count = lock.getHoldCount();
        lock.unlock(); lock.unlock();
        return count == 2 ? 1 : 0;
    }
    public static int rwlock_read() {
        ReentrantReadWriteLock rwl = new ReentrantReadWriteLock();
        rwl.readLock().lock();
        try { return rwl.getReadLockCount() == 1 ? 1 : 0; }
        finally { rwl.readLock().unlock(); }
    }
    public static int rwlock_write() {
        ReentrantReadWriteLock rwl = new ReentrantReadWriteLock();
        rwl.writeLock().lock();
        try { return rwl.isWriteLocked() ? 1 : 0; }
        finally { rwl.writeLock().unlock(); }
    }
    public static int condition_basic() {
        ReentrantLock lock = new ReentrantLock();
        Condition c = lock.newCondition();
        return c != null ? 1 : 0;
    }
    public static int stampedlock_basic() {
        StampedLock sl = new StampedLock();
        long stamp = sl.readLock();
        sl.unlockRead(stamp);
        return stamp != 0 ? 1 : 0;
    }
}
