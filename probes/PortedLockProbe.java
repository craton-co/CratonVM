import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Splits the uncontended `ReentrantLock` pair's residual in two: is it the
 * SHAPE of the code, or what the VM does to `java.util.concurrent`?
 *
 * `probes/LockEntryProbe.java` puts the real pair at ~1100 ns with exactly two
 * registered natives on it (`setExclusiveOwnerThread` x2, ~190 ns each), and
 * `probes/AqsAttributionProbe.java`'s REPLICA rung — the same ALGORITHM but
 * only three calls deep — at ~450 ns. That leaves ~700 ns, and five
 * hypotheses are already dead:
 *
 *   JIT re-entry     `jit_entries` = 2 438 for 4 000 000 pairs
 *   the interpreter  `--nojit` is 4.0x slower on this arm, 51x on the control
 *   the caller seal  `CRATONVM_JIT_NATIVE_SHADOW_CALLER_SEAL=0` engages
 *                    completely (30 seals -> 0) and moves nothing
 *   deoptimisation   `deopts=0`
 *   volatile fields  `probes/FieldAccessRungProbe.java`: volatile int
 *                    write+read is 5.5 ns here against HotSpot's 6.3
 *
 * The REPLICA is not a fair control for what is left, because it is three
 * calls deep and the real thing is eleven. This probe closes that gap: PORT is
 * the JDK's uncontended fast path — `ReentrantLock.lock` -> `Sync.lock` ->
 * `NonfairSync.initialTryLock` -> `compareAndSetState`/`setExclusiveOwnerThread`,
 * and `unlock` -> `Sync.release` -> `tryRelease` -> `getState` ->
 * `getExclusiveOwnerThread` -> `setExclusiveOwnerThread` -> `setState` ->
 * `signalNext` — at the SAME call depth, in the same order, on classes the VM
 * has no registration for and no allow-list entry about.
 *
 * The CAS is an `AtomicInteger`, deliberately: the real `compareAndSetState`
 * reaches `jdk.internal.misc.Unsafe.compareAndSetInt`, which this VM serves
 * from an intrinsic table (the census reports 0.003 dispatches per pair), and
 * `AtomicInteger` is the one CAS with an inline JIT intrinsic here. Using
 * `AtomicIntegerFieldUpdater` instead would have imported a ~250 ns registered
 * native into the control and made the comparison meaningless.
 *
 *   PORT ~= REAL  ->  it is the SHAPE. Eleven calls and a CAS simply cost this
 *                     much here, and the gap to HotSpot's 11 ns is inlining.
 *   PORT << REAL  ->  it is the CLASSES. Something the VM does specifically to
 *                     `java.util.concurrent.locks` is the residual, and the
 *                     difference is its size.
 *
 *   javac -d out probes/PortedLockProbe.java
 *   java      -cp out PortedLockProbe     # control
 *   cratonvm --java-home $JDK -cp out PortedLockProbe
 */
public final class PortedLockProbe {

    private static final int PASSES = 4;
    private static final int ROUNDS = 1_000_000;

    private static long sink;

    // ---- the port -------------------------------------------------------

    /** `AbstractOwnableSynchronizer`. */
    static abstract class PortOwnable {
        private Thread exclusiveOwnerThread;

        protected final void setExclusiveOwnerThread(Thread t) { exclusiveOwnerThread = t; }

        protected final Thread getExclusiveOwnerThread() { return exclusiveOwnerThread; }
    }

    /** `AbstractQueuedSynchronizer`, uncontended fast path only. */
    static abstract class PortQueued extends PortOwnable {
        // The real one is a `volatile int` CAS'd through `Unsafe`; an
        // `AtomicInteger` is the closest thing here whose CAS is an inline JIT
        // intrinsic rather than a registered native. See the header.
        final AtomicInteger state = new AtomicInteger();
        volatile Object head;
        volatile Object tail;

        final int getState() { return state.get(); }

        final void setState(int v) { state.set(v); }

        final boolean compareAndSetState(int expect, int update) {
            return state.compareAndSet(expect, update);
        }

        abstract boolean tryAcquire(int acquires);

        abstract boolean tryRelease(int releases);

        final void acquire(int arg) {
            if (!tryAcquire(arg)) {
                throw new IllegalStateException("uncontended path only");
            }
        }

        final boolean release(int arg) {
            if (tryRelease(arg)) {
                signalNext(head);
                return true;
            }
            return false;
        }

        final void signalNext(Object h) {
            if (h != null) {
                sink++;
            }
        }
    }

    /** `ReentrantLock$Sync`. */
    static abstract class PortSync extends PortQueued {
        abstract void lock();

        final boolean tryRelease(int releases) {
            int c = getState() - releases;
            if (getExclusiveOwnerThread() != Thread.currentThread()) {
                throw new IllegalMonitorStateException();
            }
            boolean free = (c == 0);
            if (free) {
                setExclusiveOwnerThread(null);
            }
            setState(c);
            return free;
        }

        final boolean tryLock() {
            Thread current = Thread.currentThread();
            int c = getState();
            if (c == 0) {
                if (compareAndSetState(0, 1)) {
                    setExclusiveOwnerThread(current);
                    return true;
                }
            } else if (getExclusiveOwnerThread() == current) {
                setState(c + 1);
                return true;
            }
            return false;
        }
    }

    /** `ReentrantLock$NonfairSync`. */
    static final class PortNonfairSync extends PortSync {
        final boolean initialTryLock() {
            Thread current = Thread.currentThread();
            if (compareAndSetState(0, 1)) {
                setExclusiveOwnerThread(current);
                return true;
            } else if (getExclusiveOwnerThread() == current) {
                int c = getState() + 1;
                setState(c);
                return true;
            }
            return false;
        }

        final void lock() {
            if (!initialTryLock()) {
                acquire(1);
            }
        }

        final boolean tryAcquire(int acquires) {
            if (getState() == 0 && compareAndSetState(0, acquires)) {
                setExclusiveOwnerThread(Thread.currentThread());
                return true;
            }
            return false;
        }
    }

    /** `ReentrantLock`. */
    static final class PortLock {
        private final PortSync sync = new PortNonfairSync();

        void lock() { sync.lock(); }

        void unlock() { sync.release(1); }
    }

    // ---- rungs ----------------------------------------------------------

    private static long rControl(int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { a += i; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rPort(PortLock l, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            l.lock();
            a += i;
            l.unlock();
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rReal(ReentrantLock l, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            l.lock();
            a += i;
            l.unlock();
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    private interface Rung { long run(int n); }

    private static void pass(String label, Rung r) {
        System.out.printf("%-28s", label);
        for (int p = 0; p < PASSES; p++) {
            System.out.printf("%11.1f", r.run(ROUNDS) / (double) ROUNDS);
        }
        System.out.println();
    }

    public static void main(String[] args) {
        PortLock port = new PortLock();
        ReentrantLock real = new ReentrantLock();
        System.out.printf("%-28s%11s%11s%11s%11s%n", "rung", "1", "2", "3", "4");
        pass("control: empty loop", n -> rControl(n));
        pass("PORT lock+unlock", n -> rPort(port, n));
        pass("REAL ReentrantLock pair", n -> rReal(real, n));
        System.out.println("sink=" + (sink == 0 ? 1 : 0));
    }
}
