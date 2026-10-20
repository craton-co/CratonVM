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
 * gci i15/vh: parked virtual threads across G1 CONCURRENT cycles (initial mark,
 * concurrent mark, remark, cleanup, mixed pauses) with no System.gc(), for the G1
 * half of {@code CRATONVM_GC_VT_PARKED_MIRROR_KEYED} (item 6 of
 * gci-i11vt-virtual-thread-continuation-roots-review-findings-20261013).
 *
 * Mode (stdout is a pure function of the args on HotSpot; diagnostics on stderr):
 *
 *   cycles [N L R G MB]
 *       N virtual threads park, each reachable ONLY through a heap path (id % 3 == 0:
 *       a SynchronousQueue it takes from, held by a static array; 1: a ReentrantLock
 *       main holds; 2: an executor kept in a static, woken by its shutdownNow()). Kinds
 *       0 and 1 were started by an executor that is then dropped. Each frame holds an
 *       8-node chain and an int[]. L more virtual threads, started by another dropped
 *       executor, park forever and are unreachable (counted on stderr only: whether a
 *       concurrent cycle collects them depends on timing). Main keeps MB MiB of 16 KiB
 *       arrays and replaces a quarter of them per round (old garbage: concurrent cycles
 *       and mixed pauses), plus G short-lived allocations per round, for R rounds. Then
 *       every reachable thread is released and checks its graph. Prints
 *       "vh15 cycles threads=N rounds=R lost=0 mismatches=0 checksum=ok".
 */
public class GciVh15G1ParkedCycleProbe {
    static volatile Object sink;

    static void garbage(int n) {
        Object[] keep = new Object[64];
        for (int i = 0; i < n; i++) {
            keep[i & 63] = new int[8 + (i & 15)];
        }
        sink = keep;
    }

    static final ReentrantLock LOCK = new ReentrantLock();
    static SynchronousQueue<Integer>[] QS;
    static ExecutorService KEPT;
    static final AtomicLong SUM = new AtomicLong();
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static CountDownLatch READY;
    static CountDownLatch DONE;
    static CountDownLatch UNREACHABLE_READY;
    static WeakReference<Thread>[] UNREACHABLE;

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

    static void unreachableWorker(int i) {
        int[] mine = new int[] { i, 3, 5 };
        UNREACHABLE[i] = new WeakReference<>(Thread.currentThread());
        UNREACHABLE_READY.countDown();
        while (true) {
            LockSupport.park();
            if (mine[0] == -1) {
                sink = mine;
            }
        }
    }

    // Its own frame: the executors of kinds 0 and 1 and of the unreachable
    // threads die with it.
    static void startWorkers(int n, int unreachable) {
        ExecutorService abandoned = Executors.newVirtualThreadPerTaskExecutor();
        for (int id = 0; id < n; id++) {
            final int fid = id;
            if (id % 3 == 2) {
                KEPT.submit(() -> worker(fid));
            } else {
                abandoned.submit(() -> worker(fid));
            }
        }
        ExecutorService dropped = Executors.newVirtualThreadPerTaskExecutor();
        for (int i = 0; i < unreachable; i++) {
            final int fi = i;
            dropped.submit(() -> unreachableWorker(fi));
        }
    }

    @SuppressWarnings("unchecked")
    static void cycles(int n, int unreachable, int rounds, int perRound, int ballastMb)
            throws Exception {
        QS = (SynchronousQueue<Integer>[]) new SynchronousQueue[n];
        for (int i = 0; i < n; i++) {
            QS[i] = new SynchronousQueue<>();
        }
        READY = new CountDownLatch(n);
        DONE = new CountDownLatch(n);
        UNREACHABLE = (WeakReference<Thread>[]) new WeakReference[unreachable];
        UNREACHABLE_READY = new CountDownLatch(unreachable);
        KEPT = Executors.newVirtualThreadPerTaskExecutor();
        LOCK.lock();
        startWorkers(n, unreachable);
        READY.await();
        UNREACHABLE_READY.await();
        // Let every thread reach its park.
        Thread.sleep(300);
        int chunks = ballastMb * 64;
        byte[][] ballast = new byte[chunks][];
        for (int i = 0; i < chunks; i++) {
            ballast[i] = new byte[16 * 1024 - 64];
        }
        int cursor = 0;
        for (int r = 0; r < rounds; r++) {
            garbage(perRound);
            for (int k = 0; k < chunks / 4; k++) {
                byte[] b = new byte[16 * 1024 - 64];
                b[0] = (byte) r;
                ballast[cursor] = b;
                cursor = (cursor + 1) % chunks;
            }
            Thread.sleep(5);
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
        int collected = 0;
        for (int i = 0; i < unreachable; i++) {
            if (UNREACHABLE[i] != null && UNREACHABLE[i].get() == null) {
                collected++;
            }
        }
        System.out.println("vh15 cycles threads=" + n + " rounds=" + rounds + " lost=" + lost
                + " mismatches=" + MISMATCH.get() + " checksum=" + (SUM.get() == expected ? "ok" : "bad"));
        System.err.println("[vh15] cycles sum=" + SUM.get() + " expected=" + expected
                + " unreachable_collected=" + collected + " of=" + unreachable
                + " ballast_chunks=" + chunks);
        sink = ballast;
        System.exit(0);
    }

    static int arg(String[] a, int i, int d) {
        return a.length > i ? Integer.parseInt(a[i]) : d;
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "cycles";
        switch (mode) {
            case "cycles":
                cycles(arg(args, 1, 300), arg(args, 2, 50), arg(args, 3, 200), arg(args, 4, 50000),
                        arg(args, 5, 24));
                break;
            default:
                throw new IllegalArgumentException("mode: cycles [N L R G MB]");
        }
    }
}
