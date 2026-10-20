import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;
import java.util.concurrent.locks.*;

/**
 * Regression: threading + java.util.concurrent primitives (a light smoke).
 *
 * Deliberately low-volume: heavily-contended loops that JIT-compile while the
 * GC runs hit a documented CratonVM gap (cross-thread JIT-frame root scanning
 * at a stop-the-world pause — see README "Known gaps"), so this validates that
 * the concurrency *APIs* work (threads, atomics, locks, executors, futures,
 * latches, ConcurrentHashMap) without driving that heavy path.
 *
 * NOT SCHEDULED by regression-suite/run.sh — it is named in UNREGISTERED_CLASSES
 * for that same gap. Needs no launcher flags: run it with
 * `ONLY="RConcurrent" bash regression-suite/run.sh`.
 *
 * Every wait here is BOUNDED, so a wedge fails as a named AssertionError at
 * rc=1 instead of running out the suite's own TIMEOUT with no diagnosis. The
 * bounds are hang detectors, not performance assertions: they are set far above
 * any plausible honest duration, and together stay under run.sh's 120 s default.
 */
public class RConcurrent {
    static int checks = 0;
    /** Hang-detector bound, seconds. Generous on purpose — see the class doc. */
    static final int WAIT_S = 30;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    public static void main(String[] a) throws Exception {
        final int T = 4, N = 250;

        // ---- threads + AtomicLong ----
        AtomicLong al = new AtomicLong();
        runThreads(T, () -> { for (int i = 0; i < N; i++) al.incrementAndGet(); });
        check(al.get() == (long) T * N, "AtomicLong sum: " + al.get());

        // ---- synchronized + ReentrantLock ----
        int[] box = new int[1];
        Object lock = new Object();
        runThreads(T, () -> { for (int i = 0; i < N; i++) synchronized (lock) { box[0]++; } });
        check(box[0] == T * N, "synchronized counter");
        ReentrantLock rl = new ReentrantLock();
        long[] sum = new long[1];
        runThreads(T, () -> { for (int i = 0; i < N; i++) { rl.lock(); try { sum[0]++; } finally { rl.unlock(); } } });
        check(sum[0] == (long) T * N, "ReentrantLock counter");

        // ---- single-thread atomic API surface ----
        AtomicInteger ai = new AtomicInteger(10);
        check(ai.getAndIncrement() == 10 && ai.get() == 11, "AtomicInteger getAndIncrement");
        check(ai.compareAndSet(11, 20) && ai.get() == 20, "AtomicInteger CAS");
        check(ai.addAndGet(5) == 25, "AtomicInteger addAndGet");

        // ---- ConcurrentHashMap (small) ----
        ConcurrentHashMap<Integer, Integer> chm = new ConcurrentHashMap<>();
        for (int i = 0; i < 100; i++) chm.merge(i % 10, 1, Integer::sum);
        check(chm.size() == 10 && chm.get(0) == 10, "ConcurrentHashMap merge");
        check(chm.computeIfAbsent(99, k -> 7) == 7, "CHM computeIfAbsent");

        // ---- ExecutorService + Future ----
        ExecutorService ex = Executors.newFixedThreadPool(4);
        List<Future<Integer>> fs = new ArrayList<>();
        for (int i = 0; i < 20; i++) { final int k = i; fs.add(ex.submit(() -> k * k)); }
        int fsum = 0; for (Future<Integer> f : fs) fsum += f.get();
        ex.shutdown();
        check(ex.awaitTermination(WAIT_S, TimeUnit.SECONDS), "executor terminates");
        check(fsum == 2470, "ExecutorService futures sum"); // sum_{0..19} k^2

        // ---- CountDownLatch ----
        CountDownLatch latch = new CountDownLatch(T);
        AtomicInteger done = new AtomicInteger();
        for (int i = 0; i < T; i++) new Thread(() -> { done.incrementAndGet(); latch.countDown(); }).start();
        check(latch.await(WAIT_S, TimeUnit.SECONDS) && done.get() == T, "CountDownLatch");

        // ---- CompletableFuture ----
        CompletableFuture<Integer> cf = CompletableFuture.supplyAsync(() -> 21).thenApply(x -> x * 2);
        check(cf.get(WAIT_S, TimeUnit.SECONDS) == 42, "CompletableFuture chain");

        System.out.println("PASS RConcurrent (" + checks + " checks)");
    }

    static void runThreads(int n, Runnable body) throws InterruptedException {
        Thread[] ts = new Thread[n];
        for (int i = 0; i < n; i++) (ts[i] = new Thread(body)).start();
        for (Thread t : ts) t.join();
    }
}
