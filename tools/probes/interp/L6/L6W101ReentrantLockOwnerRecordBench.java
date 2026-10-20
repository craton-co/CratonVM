/*
 * Interpreter round i5, wave 101, lane L6: the price of the AQS owner record
 * the `ReentrantLock` fast path still wrote after the per-lock owner index
 * was retired (`CRATONVM_OWNER_RECORD_NEEDS_INDEX`, default OFF;
 * page i77-L1-proposal-a-per-thread-owned-synchronizer-record, Wave 101).
 *
 * A/B, interleaved, 5 reps, both arms with the default ReentrantLock fast
 * path (on by default without the JIT):
 *
 *   for rep in 1 2 3 4 5; do for SW in CRATONVM_NONE=1 CRATONVM_OWNER_RECORD_NEEDS_INDEX=1; do
 *     env $SW cratonvm --nojit -cp out L6W101ReentrantLockOwnerRecordBench
 *     env $SW cratonvm         -cp out L6W101ReentrantLockOwnerRecordBench
 *   done; done
 *
 * Rows (TIME lines, ns per lock/unlock pair, best of ROUNDS):
 *   uncontended  one thread, one lock
 *   reentrant    lock twice, unlock twice (only the outer pair records)
 *   two-locks    alternate two locks (the old index's shard map, two keys)
 *   shared-2t    two threads, a lock each, run at once (the old index's
 *                shared shard mutexes were the contention the record cost)
 *
 * Deterministic stdout (identical on HotSpot 25): the fidelity rows the
 * switch must not move -- getLockedSynchronizers while held and after
 * release (answered by the dump-time heap walk either way), and a
 * ReentrantLock deadlock found by findDeadlockedThreads.
 */
import java.lang.management.LockInfo;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadInfo;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.ReentrantLock;

public class L6W101ReentrantLockOwnerRecordBench {
    static final int N = 300_000;
    static final int ROUNDS = 5;
    static volatile long sink;

    public static void main(String[] args) throws Exception {
        fidelity();
        bench();
    }

    static String names(LockInfo[] infos) {
        StringBuilder sb = new StringBuilder("[");
        for (int i = 0; i < infos.length; i++) {
            if (i > 0) sb.append(", ");
            sb.append(infos[i].getClassName());
        }
        return sb.append(']').toString();
    }

    static LockInfo[] mine(ThreadMXBean mx) {
        ThreadInfo[] infos = mx.getThreadInfo(new long[] {Thread.currentThread().threadId()}, true, true);
        return infos[0].getLockedSynchronizers();
    }

    static void fidelity() throws Exception {
        ThreadMXBean mx = ManagementFactory.getThreadMXBean();
        ReentrantLock a = new ReentrantLock();
        ReentrantLock f = new ReentrantLock(true);
        a.lock();
        System.out.println("held one: " + names(mine(mx)));
        f.lock();
        a.lock();
        System.out.println("held two (a reentered): " + mine(mx).length);
        a.unlock();
        a.unlock();
        System.out.println("held fair only: " + names(mine(mx)));
        f.unlock();
        System.out.println("released: " + names(mine(mx)));
        // A release made by the JDK body: a queued successor makes the fast
        // path decline the unlock.
        ReentrantLock q = new ReentrantLock();
        CountDownLatch queued = new CountDownLatch(1);
        q.lock();
        Thread waiter = new Thread(() -> {
            queued.countDown();
            q.lock();
            q.unlock();
        }, "waiter");
        waiter.start();
        queued.await();
        while (!q.hasQueuedThread(waiter)) Thread.onSpinWait();
        q.unlock();
        waiter.join();
        System.out.println("after a queued hand-off: " + names(mine(mx)));
        System.out.println("isLocked after hand-off: " + q.isLocked());

        // A ReentrantLock deadlock.
        ReentrantLock l1 = new ReentrantLock();
        ReentrantLock l2 = new ReentrantLock();
        CountDownLatch both = new CountDownLatch(2);
        Thread t1 = new Thread(() -> {
            l1.lock();
            both.countDown();
            try { both.await(); } catch (InterruptedException e) { return; }
            l2.lock();
        }, "dl-1");
        Thread t2 = new Thread(() -> {
            l2.lock();
            both.countDown();
            try { both.await(); } catch (InterruptedException e) { return; }
            l1.lock();
        }, "dl-2");
        t1.setDaemon(true);
        t2.setDaemon(true);
        t1.start();
        t2.start();
        long[] found = null;
        for (int i = 0; i < 2000 && found == null; i++) {
            found = mx.findDeadlockedThreads();
            if (found == null) Thread.sleep(5);
        }
        System.out.println("deadlocked: " + (found == null ? "none" : found.length));
        if (found != null) {
            ThreadInfo[] infos = mx.getThreadInfo(found, true, true);
            java.util.Arrays.sort(infos, (x, y) -> x.getThreadName().compareTo(y.getThreadName()));
            for (ThreadInfo ti : infos) {
                System.out.println("  " + ti.getThreadName() + " waits " + ti.getLockInfo().getClassName()
                        + " owned by " + ti.getLockOwnerName() + ", holds " + names(ti.getLockedSynchronizers()));
            }
        }
    }

    static long uncontended(ReentrantLock l) {
        long t0 = System.nanoTime();
        for (int i = 0; i < N; i++) {
            l.lock();
            l.unlock();
        }
        return System.nanoTime() - t0;
    }

    static long reentrant(ReentrantLock l) {
        long t0 = System.nanoTime();
        for (int i = 0; i < N; i++) {
            l.lock();
            l.lock();
            l.unlock();
            l.unlock();
        }
        return System.nanoTime() - t0;
    }

    static long twoLocks(ReentrantLock a, ReentrantLock b) {
        long t0 = System.nanoTime();
        for (int i = 0; i < N; i++) {
            ReentrantLock l = (i & 1) == 0 ? a : b;
            l.lock();
            l.unlock();
        }
        return System.nanoTime() - t0;
    }

    static long shared2t() throws Exception {
        CountDownLatch go = new CountDownLatch(1);
        long[] took = new long[2];
        Thread[] ts = new Thread[2];
        for (int k = 0; k < 2; k++) {
            final int id = k;
            ts[k] = new Thread(() -> {
                ReentrantLock own = new ReentrantLock();
                try { go.await(); } catch (InterruptedException e) { return; }
                took[id] = uncontended(own);
            });
            ts[k].start();
        }
        go.countDown();
        for (Thread t : ts) t.join();
        return Math.max(took[0], took[1]);
    }

    static void bench() throws Exception {
        ReentrantLock a = new ReentrantLock();
        ReentrantLock b = new ReentrantLock();
        long bu = Long.MAX_VALUE, br = Long.MAX_VALUE, bt = Long.MAX_VALUE, bs = Long.MAX_VALUE;
        for (int r = 0; r < ROUNDS; r++) {
            bu = Math.min(bu, uncontended(a));
            br = Math.min(br, reentrant(a));
            bt = Math.min(bt, twoLocks(a, b));
            bs = Math.min(bs, shared2t());
        }
        sink = bu + br + bt + bs;
        System.out.println("bench done: locked=" + a.isLocked() + " " + b.isLocked());
        System.out.println("TIME uncontended " + (bu / N) + " ns/pair");
        System.out.println("TIME reentrant " + (br / N) + " ns/outer-pair");
        System.out.println("TIME two-locks " + (bt / N) + " ns/pair");
        System.out.println("TIME shared-2t " + (bs / N) + " ns/pair");
    }
}
