import java.lang.reflect.Method;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

/**
 * gci i17/vt: do N virtual threads that block on something main holds all get to
 * run their first statement? (page
 * gci-i15orch-virtual-threads-never-start-under-nojit-so-300-thread-probes-hang-FIXED-20261015).
 *
 * Every worker counts STARTED down as its first statement, then blocks until main
 * releases it, then counts DONE down. HotSpot 25 unmounts every one of these blocks,
 * so all N start whatever N is. A VM that pins a block to its carrier instead runs at
 * most "carriers" workers to their block, and the rest never start. Main waits at most
 * WAIT seconds for the starts (it never hangs), releases everyone, waits again, and
 * prints
 *
 *   vt17 MODE threads=N started=S done=D
 *
 * (stdout is a pure function of the args on HotSpot: S = D = N). Diagnostics on stderr.
 *
 * Modes:
 *   lock     ReentrantLock.lock() on a lock main holds (the contended lock() the
 *            no-JIT ReentrantLock intrinsic re-enters its JDK body for).
 *   reflect  the same lock(), reached through Method.invoke (a reflective call with
 *            the park beneath it).
 *   queue    SynchronousQueue.take(), main offers one item per worker.
 *   park     LockSupport.park() until main unparks the thread.
 *   mixed    id % 3: queue, lock, park (the ve14 "reachable" shape without the GC).
 *
 * Usage: GciVt17PinnedStartProbe MODE [N [WAIT]]   (N default 300, WAIT default 60)
 */
public class GciVt17PinnedStartProbe {
    static final ReentrantLock LOCK = new ReentrantLock();
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static CountDownLatch STARTED;
    static CountDownLatch DONE;
    static SynchronousQueue<Integer>[] QS;
    static Thread[] THREADS;
    static volatile boolean released;
    static Method LOCK_METHOD;

    static void lockOnce() {
        LOCK.lock();
        LOCK.unlock();
    }

    static void viaReflection() {
        try {
            LOCK_METHOD.invoke(null);
        } catch (ReflectiveOperationException e) {
            MISMATCH.incrementAndGet();
        }
    }

    static void takeOne(int id) {
        try {
            Integer got = QS[id].take();
            if (got.intValue() != id) {
                MISMATCH.incrementAndGet();
            }
        } catch (InterruptedException e) {
            MISMATCH.incrementAndGet();
        }
    }

    static void parkUntilReleased() {
        while (!released) {
            LockSupport.park();
        }
    }

    static int kindOf(String mode, int id) {
        switch (mode) {
            case "queue":
                return 0;
            case "lock":
                return 1;
            case "park":
                return 2;
            case "reflect":
                return 3;
            case "mixed":
                return id % 3;
            default:
                throw new IllegalArgumentException("mode: lock|reflect|queue|park|mixed");
        }
    }

    static void worker(int kind, int id) {
        STARTED.countDown();
        switch (kind) {
            case 0:
                takeOne(id);
                break;
            case 1:
                lockOnce();
                break;
            case 2:
                parkUntilReleased();
                break;
            default:
                viaReflection();
                break;
        }
        DONE.countDown();
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "lock";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 300;
        int wait = args.length > 2 ? Integer.parseInt(args[2]) : 60;
        LOCK_METHOD = GciVt17PinnedStartProbe.class.getDeclaredMethod("lockOnce");
        QS = (SynchronousQueue<Integer>[]) new SynchronousQueue[n];
        for (int i = 0; i < n; i++) {
            QS[i] = new SynchronousQueue<>();
        }
        THREADS = new Thread[n];
        STARTED = new CountDownLatch(n);
        DONE = new CountDownLatch(n);
        ThreadFactory factory = Thread.ofVirtual().factory();
        LOCK.lock();
        long t0 = System.nanoTime();
        for (int id = 0; id < n; id++) {
            final int fid = id;
            final int kind = kindOf(mode, id);
            THREADS[id] = factory.newThread(() -> worker(kind, fid));
            THREADS[id].start();
        }
        STARTED.await(wait, TimeUnit.SECONDS);
        long started = n - STARTED.getCount();
        long t1 = System.nanoTime();
        // Release everyone, started or not.
        released = true;
        LOCK.unlock();
        // One deadline for all the offers: a worker that never started must not
        // cost a full WAIT each.
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(wait);
        for (int id = 0; id < n; id++) {
            int kind = kindOf(mode, id);
            if (kind == 0) {
                long left = Math.max(0L, deadline - System.nanoTime());
                if (!QS[id].offer(id, left, TimeUnit.NANOSECONDS)) {
                    MISMATCH.incrementAndGet();
                }
            } else if (kind == 2) {
                LockSupport.unpark(THREADS[id]);
            }
        }
        DONE.await(wait, TimeUnit.SECONDS);
        long done = n - DONE.getCount();
        System.out.println("vt17 " + mode + " threads=" + n + " started=" + started + " done=" + done);
        System.err.println("[vt17] " + mode + " mismatches=" + MISMATCH.get()
                + " start_ms=" + (t1 - t0) / 1_000_000
                + " carriers=" + Runtime.getRuntime().availableProcessors());
        System.exit(0);
    }
}
