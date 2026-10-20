import java.lang.ref.WeakReference;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.locks.LockSupport;
import java.util.concurrent.locks.ReentrantLock;

/**
 * gci i14/ve: parked virtual threads keyed by their mirrors
 * ({@code CRATONVM_GC_VT_PARKED_MIRROR_KEYED}), item 6 of
 * gci-i11vt-virtual-thread-continuation-roots-review-findings-20261013.
 *
 * Modes (stdout is a pure function of the args on HotSpot; diagnostics on stderr):
 *
 *   executor [R G]
 *       A virtual thread started by an ABANDONED newVirtualThreadPerTaskExecutor
 *       parks forever, holding an int[] only its frame names. The executor, not
 *       the root container, tracks it, and the executor is registered only weakly,
 *       so on HotSpot both are collectable. R rounds of G allocations plus
 *       System.gc() (enough for the parked frames to age into the old generation),
 *       then: "ve14 executor parked thread collected=<b> frame-only object collected=<b>".
 *
 *   root [R G]
 *       The same thread started by Thread.ofVirtual().start(): kept by the root
 *       container by default; collectable under -Djdk.trackAllThreads=false.
 *       Prints "ve14 root parked thread collected=<b> frame-only object collected=<b>".
 *
 *   reachable [N R G]
 *       Soundness: N virtual threads park, each reachable ONLY through a heap
 *       path (id % 3 == 0: a SynchronousQueue it takes from, held by a static
 *       array; 1: a ReentrantLock main holds; 2: an executor kept in a static,
 *       woken by its shutdownNow()). Kinds 0 and 1 were started by an executor
 *       that is then dropped. Each frame holds an 8-node chain and an int[]. R
 *       rounds of G allocations plus System.gc(), then every thread is released
 *       and checks its graph. Prints
 *       "ve14 reachable threads=N rounds=R lost=0 mismatches=0 checksum=ok".
 */
public class GciVe14ParkedMirrorKeyedProbe {
    static volatile Object sink;

    static void garbage(int n) {
        Object[] keep = new Object[64];
        for (int i = 0; i < n; i++) {
            keep[i & 63] = new int[8 + (i & 15)];
        }
        sink = keep;
    }

    // ------------------------------------------------------------ unreachable

    static WeakReference<Thread> parkedThread;
    static WeakReference<Object> heldOnlyByFrame;

    static void parkForever(CountDownLatch started) {
        int[] mine = new int[] { 7, 11, 13 };
        heldOnlyByFrame = new WeakReference<>(mine);
        parkedThread = new WeakReference<>(Thread.currentThread());
        started.countDown();
        while (true) {
            LockSupport.park();
            if (mine[0] == 0x7fffffff) {
                sink = mine;
            }
        }
    }

    // Its own frame: the executor and the Future die with it.
    static void startAbandoned(boolean root) throws Exception {
        CountDownLatch started = new CountDownLatch(1);
        if (root) {
            Thread.ofVirtual().start(() -> parkForever(started));
        } else {
            ExecutorService ex = Executors.newVirtualThreadPerTaskExecutor();
            ex.submit(() -> parkForever(started));
        }
        started.await();
        // Let the thread reach its park.
        Thread.sleep(200);
    }

    static void unreachable(String mode, int rounds, int perRound) throws Exception {
        startAbandoned(mode.equals("root"));
        int firstCollected = -1;
        for (int r = 0; r < rounds; r++) {
            garbage(perRound);
            System.gc();
            Thread.sleep(20);
            if (firstCollected < 0 && parkedThread.get() == null) {
                firstCollected = r;
            }
        }
        System.out.println("ve14 " + mode + " parked thread collected=" + (parkedThread.get() == null)
                + " frame-only object collected=" + (heldOnlyByFrame.get() == null));
        System.err.println("[ve14] " + mode + " rounds=" + rounds + " first_collected_round="
                + firstCollected);
        System.exit(0);
    }

    // ------------------------------------------------------------ reachable

    static final ReentrantLock LOCK = new ReentrantLock();
    static SynchronousQueue<Integer>[] QS;
    static ExecutorService KEPT;
    static final AtomicLong SUM = new AtomicLong();
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static CountDownLatch READY;
    static CountDownLatch DONE;

    static final class Node {
        final int v;
        final Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    static Node chain(int id) {
        Node n = null;
        for (int i = 0; i < 8; i++) {
            n = new Node(id * 31 + i, n);
        }
        return n;
    }

    static long check(int id, Node n, int[] payload) {
        long s = 0;
        int i = 7;
        for (Node c = n; c != null; c = c.next) {
            if (c.v != id * 31 + i) {
                MISMATCH.incrementAndGet();
            }
            s += c.v;
            i--;
        }
        if (i != -1) {
            MISMATCH.incrementAndGet();
        }
        if (payload.length != 3 + (id & 7) || payload[0] != id || payload[1] != id * 7) {
            MISMATCH.incrementAndGet();
        }
        return s + payload[1];
    }

    static long expectedOf(int id) {
        // sum over i < 8 of (id * 31 + i), plus id * 7
        return 255L * id + 28L;
    }

    static void worker(int id) {
        Node n = chain(id);
        int[] payload = new int[3 + (id & 7)];
        payload[0] = id;
        payload[1] = id * 7;
        try {
            switch (id % 3) {
                case 0: {
                    READY.countDown();
                    Integer got = QS[id].take();
                    if (got.intValue() != id) {
                        MISMATCH.incrementAndGet();
                    }
                    break;
                }
                case 1: {
                    READY.countDown();
                    LOCK.lock();
                    LOCK.unlock();
                    break;
                }
                default: {
                    READY.countDown();
                    while (!Thread.currentThread().isInterrupted()) {
                        LockSupport.park();
                    }
                    break;
                }
            }
        } catch (InterruptedException e) {
            MISMATCH.incrementAndGet();
        }
        SUM.addAndGet(check(id, n, payload));
        DONE.countDown();
    }

    // Its own frame: the executor of kinds 0 and 1 dies with it.
    static void startWorkers(int n) {
        ExecutorService abandoned = Executors.newVirtualThreadPerTaskExecutor();
        for (int id = 0; id < n; id++) {
            final int fid = id;
            if (id % 3 == 2) {
                KEPT.submit(() -> worker(fid));
            } else {
                abandoned.submit(() -> worker(fid));
            }
        }
    }

    @SuppressWarnings("unchecked")
    static void reachable(int n, int rounds, int perRound) throws Exception {
        QS = (SynchronousQueue<Integer>[]) new SynchronousQueue[n];
        for (int i = 0; i < n; i++) {
            QS[i] = new SynchronousQueue<>();
        }
        READY = new CountDownLatch(n);
        DONE = new CountDownLatch(n);
        KEPT = Executors.newVirtualThreadPerTaskExecutor();
        LOCK.lock();
        startWorkers(n);
        READY.await();
        // Let every thread reach its park.
        Thread.sleep(300);
        for (int r = 0; r < rounds; r++) {
            garbage(perRound);
            System.gc();
            Thread.sleep(10);
        }
        for (int i = 0; i < n; i += 3) {
            if (!QS[i].offer(i, 10, TimeUnit.SECONDS)) {
                MISMATCH.incrementAndGet();
            }
        }
        LOCK.unlock();
        KEPT.shutdownNow();
        DONE.await(60, TimeUnit.SECONDS);
        long lost = DONE.getCount();
        long expected = 0;
        for (int id = 0; id < n; id++) {
            expected += expectedOf(id);
        }
        System.out.println("ve14 reachable threads=" + n + " rounds=" + rounds + " lost=" + lost
                + " mismatches=" + MISMATCH.get() + " checksum=" + (SUM.get() == expected ? "ok" : "bad"));
        System.err.println("[ve14] reachable sum=" + SUM.get() + " expected=" + expected);
        System.exit(0);
    }

    static int arg(String[] a, int i, int d) {
        return a.length > i ? Integer.parseInt(a[i]) : d;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "executor";
        switch (mode) {
            case "executor":
            case "root":
                unreachable(mode, arg(args, 1, 12), arg(args, 2, 400000));
                break;
            case "reachable":
                reachable(arg(args, 1, 300), arg(args, 2, 12), arg(args, 3, 200000));
                break;
            default:
                throw new IllegalArgumentException("mode: executor|root [R G] | reachable [N R G]");
        }
    }
}
