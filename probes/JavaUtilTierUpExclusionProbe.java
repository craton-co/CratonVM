import java.util.concurrent.locks.ReentrantLock;

/**
 * Is `java.util.concurrent` being excluded from virtual tier-up by a prefix
 * test meant for `java.util` COLLECTIONS?
 *
 * `execute_invokevirtual_cached` suppresses invocation-count tier-up when the
 * RECEIVER's class name `starts_with("java/util/")`. The comment says the
 * hazard is a hot java.util graph Spring reaches while building annotation and
 * conversion metadata — collections. But the prefix also matches
 * `java/util/concurrent/locks/ReentrantLock`,
 * `java/util/concurrent/locks/AbstractQueuedSynchronizer`,
 * `java/util/concurrent/ThreadPoolExecutor` … i.e. the whole concurrency stack.
 *
 * The gate reads the RECEIVER's class, not the call site's symbolic owner. So a
 * user-defined subclass of ReentrantLock has a receiver class outside
 * `java/util/` while running byte-for-byte the same inherited `lock()`/
 * `unlock()` bodies. If the subclass is dramatically faster, the prefix is the
 * blocker — no VM rebuild needed to find out.
 *
 * Prints ns/op for both. Same work, same bytecode, different receiver class.
 */
public class JavaUtilTierUpExclusionProbe {

    /** Identical behaviour; the only difference is the receiver's class name. */
    private static final class MyLock extends ReentrantLock {
        private static final long serialVersionUID = 1L;
    }

    private static final int WARMUP = 200_000;
    private static final int ROUNDS = 2_000_000;

    // TWO methods, deliberately. A single `lockUnlock(ReentrantLock, int)`
    // called with both receivers turns its `lock.lock()` site POLYMORPHIC, and
    // a poly site costs ~6x a monomorphic one here — which is what the first
    // cut of this probe actually measured (the subclass came out 2.5x SLOWER,
    // an artifact of the shared call site, not of the receiver's class name).
    // Each loop below sees exactly one receiver class.

    private static long lockUnlockPlain(ReentrantLock lock, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            lock.unlock();
        }
        return System.nanoTime() - t0;
    }

    private static long lockUnlockSub(MyLock lock, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            lock.lock();
            lock.unlock();
        }
        return System.nanoTime() - t0;
    }

    public static void main(String[] args) {
        ReentrantLock plain = new ReentrantLock();
        MyLock sub = new MyLock();

        // Warm both, in both orders, before either is measured.
        lockUnlockPlain(plain, WARMUP);
        lockUnlockSub(sub, WARMUP);
        lockUnlockPlain(plain, WARMUP);
        lockUnlockSub(sub, WARMUP);

        long p1 = lockUnlockPlain(plain, ROUNDS);
        long s1 = lockUnlockSub(sub, ROUNDS);
        long s2 = lockUnlockSub(sub, ROUNDS);
        long p2 = lockUnlockPlain(plain, ROUNDS);

        double plainNs = (p1 + p2) / (2.0 * ROUNDS);
        double subNs = (s1 + s2) / (2.0 * ROUNDS);
        System.out.printf("ReentrantLock       (receiver java/util/...)  %9.1f ns/op%n", plainNs);
        System.out.printf("MyLock extends it   (receiver NOT java/util)  %9.1f ns/op%n", subNs);
        System.out.printf("subclass is %.2fx the speed of the base class%n", plainNs / subNs);
    }
}
