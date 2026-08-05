import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.locks.AbstractOwnableSynchronizer;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Where do the ~10.5 us of an UNCONTENDED `ReentrantLock.lock()`/`unlock()`
 * pair actually go?
 *
 * Two previous answers were wrong in the same way: each found a real, expensive
 * mechanism on the path and then assumed it was THE mechanism, without checking
 * that the terms summed to the measured total. Revision 1 blamed the pre-park
 * spin (the uncontended path never spins). Revision 2 blamed the five native
 * calls at the `safe_native_call` funnel — but 5 x 330-810 ns is ~2.5-4 us and
 * the 16 nested Java calls are ~138 ns, against a 10,502 ns pair. Two thirds
 * had no owner. See
 * `docs/internal/uncontended-reentrantlock-pair-mostly-unattributed-RETIRED-20260805.md`.
 *
 * So this probe does not ask "what is expensive". It BUILDS THE PAIR UP from
 * its parts, so every rung is a subtraction from the next:
 *
 *   control            -> the loop itself
 *   emptyCall          -> the scale (one ordinary Java call)
 *   currentThread x2   -> what AQS calls twice per pair
 *   atomicCas          -> stands in for `Unsafe.compareAndSetInt`, 1 per pair
 *   setOwner x2        -> `AbstractOwnableSynchronizer.setExclusiveOwnerThread`,
 *                         2 per pair, and the one native on the path that is
 *                         intercepted for ThreadMXBean rather than for speed
 *   replicaLockUnlock  -> the same ALGORITHM with none of the JDK's classes:
 *                         a volatile int state, one CAS through a field
 *                         updater, one plain owner field. If this is fast and
 *                         the real one is slow, the gap is in what the VM does
 *                         to `java.util.concurrent`, not in the algorithm.
 *   tryLockUnlock      -> the JDK, minus `lock()`'s extra frame
 *   lockUnlock         -> the JDK, the number being explained
 *
 * `sumOfParts` prints the arithmetic so a reader cannot skip it: if
 * `lockUnlock` minus the parts is still most of `lockUnlock`, the residual is
 * the finding and nothing here is the answer yet.
 *
 * DISCIPLINE (both traps are recorded in AqsBreakdownProbe's header, and both
 * produced confident wrong numbers first):
 *  - every rung is an inline loop in its OWN method, so no lambda or shared
 *    call site is inside the measurement;
 *  - every receiver class gets its own loop method, so no site goes bimorphic;
 *  - multi-pass, and every pass is printed. A rung that has not gone flat is
 *    not a measurement, and `emptyCall` is the load scale — if it moved between
 *    the first and last pass, throw the run away.
 *
 *   javac -d out probes/AqsAttributionProbe.java
 *   java  -cp out AqsAttributionProbe          # HotSpot control
 *   cratonvm --java-home $jdk -cp out AqsAttributionProbe
 */
public final class AqsAttributionProbe {

    private static final int PASSES = 4;
    private static final int ROUNDS = 500_000;

    private static long sink;
    private static Object osink;

    // ---- subjects -------------------------------------------------------

    /** Exposes the protected AQS ownership setter, and nothing else. */
    private static final class OwnerOnly extends AbstractOwnableSynchronizer {
        private static final long serialVersionUID = 1L;

        void set(Thread t) {
            setExclusiveOwnerThread(t);
        }
    }

    /**
     * `NonfairSync.initialTryLock()` + `Sync.tryRelease()`, rewritten against
     * classes the VM has no special opinion about: no AQS, no
     * `AbstractOwnableSynchronizer`, no `java/util/` receiver.
     *
     * Deliberately the SAME operations in the same order — read state, CAS
     * 0->1, store owner, read state, read owner, clear owner, store state —
     * so a difference against the real pair is a difference in what the VM
     * does to those classes, not in how much work is asked for.
     */
    private static final class ReplicaLock {
        private static final AtomicIntegerFieldUpdater<ReplicaLock> STATE =
                AtomicIntegerFieldUpdater.newUpdater(ReplicaLock.class, "state");

        volatile int state;
        Thread owner;

        boolean lock() {
            if (STATE.compareAndSet(this, 0, 1)) {
                owner = Thread.currentThread();
                return true;
            }
            return false;
        }

        void unlock() {
            int c = state;
            if (owner == Thread.currentThread()) {
                owner = null;
                STATE.lazySet(this, c - 1);
            }
        }
    }

    // ---- rungs (each its own inline loop, one receiver class each) -------

    private static long rControl(int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { a += i; }
        sink += a;
        return System.nanoTime() - t0;
    }

    private int payload;
    private int bump(int i) { return payload += i; }

    private static long rEmptyCall(AqsAttributionProbe p, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) { a += p.bump(1); }
        sink += a;
        return System.nanoTime() - t0;
    }

    /** Two per pair. */
    private static long rCurrentThread2(int n) {
        long t0 = System.nanoTime();
        Object o = null;
        for (int i = 0; i < n; i++) {
            o = Thread.currentThread();
            o = Thread.currentThread();
        }
        osink = o;
        return System.nanoTime() - t0;
    }

    /** One per pair — stands in for `Unsafe.compareAndSetInt`. */
    private static long rAtomicCas(AtomicInteger a, int n) {
        long t0 = System.nanoTime();
        long acc = 0;
        for (int i = 0; i < n; i++) {
            a.compareAndSet(0, 1);
            a.set(0);
            acc++;
        }
        sink += acc;
        return System.nanoTime() - t0;
    }

    /** Two per pair: acquire passes the owner, release passes null. */
    private static long rSetOwner2(OwnerOnly s, Thread self, int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            s.set(self);
            s.set(null);
        }
        return System.nanoTime() - t0;
    }

    private static long rReplica(ReplicaLock l, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            if (l.lock()) { a++; }
            l.unlock();
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rTryLock(ReentrantLock l, int n) {
        long t0 = System.nanoTime();
        long a = 0;
        for (int i = 0; i < n; i++) {
            if (l.tryLock()) { a++; l.unlock(); }
        }
        sink += a;
        return System.nanoTime() - t0;
    }

    private static long rLockUnlock(ReentrantLock l, int n) {
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

    // ---- driver ---------------------------------------------------------

    private interface Rung { long run(int n); }

    private static double pass(String label, Rung r) {
        System.out.printf("%-42s", label);
        double last = 0;
        for (int i = 0; i < PASSES; i++) {
            last = r.run(ROUNDS) / (double) ROUNDS;
            System.out.printf("%11.1f", last);
        }
        System.out.println();
        return last;
    }

    public static void main(String[] args) {
        AqsAttributionProbe p = new AqsAttributionProbe();
        AtomicInteger ai = new AtomicInteger();
        OwnerOnly owner = new OwnerOnly();
        Thread self = Thread.currentThread();
        ReplicaLock replica = new ReplicaLock();
        ReentrantLock tryLock = new ReentrantLock();
        ReentrantLock lock = new ReentrantLock();

        System.out.printf("%-42s%11s%11s%11s%11s   (ns/op per pass; read the LAST)%n",
                "rung", "1", "2", "3", "4");

        double control = pass("control: empty loop", AqsAttributionProbe::rControl);
        double empty = pass("empty instance call (the scale)", n -> rEmptyCall(p, n));
        System.out.println();
        double ct2 = pass("Thread.currentThread() x2", AqsAttributionProbe::rCurrentThread2);
        double cas = pass("AtomicInteger CAS + set (~Unsafe.CAS)", n -> rAtomicCas(ai, n));
        double owner2 = pass("setExclusiveOwnerThread x2", n -> rSetOwner2(owner, self, n));
        System.out.println();
        double rep = pass("REPLICA lock+unlock (no AQS at all)", n -> rReplica(replica, n));
        double tryl = pass("ReentrantLock tryLock+unlock", n -> rTryLock(tryLock, n));
        double full = pass("ReentrantLock lock+unlock", n -> rLockUnlock(lock, n));

        double parts = ct2 + cas + owner2;
        System.out.println();
        System.out.printf("sum of the censused parts (ct2 + cas + owner2) = %.1f ns%n", parts);
        System.out.printf("ReentrantLock lock+unlock                      = %.1f ns%n", full);
        System.out.printf("UNATTRIBUTED                                   = %.1f ns  (%.0f%% of the pair)%n",
                full - parts, 100.0 * (full - parts) / full);
        System.out.printf("replica / real                                 = %.2fx%n", full / rep);
        if (control > empty) {
            System.out.println("NOTE: control >= empty call — the loop is being eliminated; distrust this run.");
        }
        if (full < parts) {
            // Seen for real: under background load the rungs drift enough that
            // the parts, measured earlier in the run, out-total the pair
            // measured later. The arithmetic is only meaningful when the rungs
            // are drawn from the same machine state.
            System.out.println("NOTE: parts out-total the pair — the host drifted during the run; VOID, rerun it.");
        }
        if (sink == Long.MIN_VALUE) {
            System.out.println(osink);
        }
    }
}
