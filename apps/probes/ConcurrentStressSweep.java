import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

/**
 * L6 differential sweep: the concurrency package UNDER CONTENTION.
 *
 * Every earlier sweep in this lane asks single-threaded contract questions —
 * argument validation, refusals, edge values. None of them runs two threads,
 * which is what the package is for. This VM's atomics are LOCK-BASED rather
 * than hardware (see `cratonvm-atomics-are-lock-based-not-hardware-atomic`), so
 * "same answer" and "same answer under contention" are different claims.
 *
 * EVERY ROW IS AN EXACT INVARIANT, never a timing or a schedule. A lost update
 * shows as a wrong number; an interleaving does not show at all. That is what
 * makes this diffable against HotSpot rather than a flake generator:
 *
 *   - N threads x M increments must total exactly N*M
 *   - a CAS race must have exactly ONE winner
 *   - what is offered to a queue must equal what is drained from it
 *   - a lock must serialise a non-atomic read-modify-write perfectly
 *
 * Modest thread counts on purpose: this runs on a shared, loaded build host,
 * and the invariants do not get truer with more threads — they only get slower
 * to check.
 */
public class ConcurrentStressSweep {
    static final int THREADS = 4;
    static final int ITERS = 5000;

    static void p(String tag, Object v) { System.out.println(tag + " |" + v + "|"); }

    interface Body { Object run() throws Throwable; }

    /** Run `c` with a hard deadline so a lost wakeup is a ROW, not a dead sweep. */
    static void t(String tag, Body c) {
        final Object[] out = new Object[1];
        Thread th = new Thread(() -> {
            try { out[0] = c.run(); }
            catch (Throwable e) {
                String m = e.getMessage();
                out[0] = "THREW " + e.getClass().getName() + (m == null ? "" : ": " + m);
            }
        });
        th.setDaemon(true);
        th.start();
        try { th.join(60000); } catch (InterruptedException ignored) { }
        p(tag, th.isAlive() ? "TIMEOUT-60s" : String.valueOf(out[0]));
    }

    /** Start THREADS workers, release them together, join them all. */
    static void race(Runnable work) throws Exception {
        CountDownLatch start = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(THREADS);
        for (int i = 0; i < THREADS; i++) {
            Thread th = new Thread(() -> {
                try { start.await(); work.run(); }
                catch (InterruptedException ignored) { }
                finally { done.countDown(); }
            });
            th.setDaemon(true);
            th.start();
        }
        start.countDown();
        done.await(50, TimeUnit.SECONDS);
    }

    public static void main(String[] args) {
        final int total = THREADS * ITERS;

        // ---- atomic counters: every increment must survive -----------------
        t("ai.incrementAndGet.total", () -> {
            AtomicInteger a = new AtomicInteger();
            race(() -> { for (int i = 0; i < ITERS; i++) { a.incrementAndGet(); } });
            return a.get() + "/" + total;
        });
        t("ai.getAndAdd.total", () -> {
            AtomicInteger a = new AtomicInteger();
            race(() -> { for (int i = 0; i < ITERS; i++) { a.getAndAdd(2); } });
            return a.get() + "/" + (total * 2);
        });
        t("al.incrementAndGet.total", () -> {
            AtomicLong a = new AtomicLong();
            race(() -> { for (int i = 0; i < ITERS; i++) { a.incrementAndGet(); } });
            return a.get() + "/" + total;
        });
        t("la.increment.sum", () -> {
            LongAdder a = new LongAdder();
            race(() -> { for (int i = 0; i < ITERS; i++) { a.increment(); } });
            return a.sum() + "/" + total;
        });
        t("ai.updateAndGet.total", () -> {
            AtomicInteger a = new AtomicInteger();
            race(() -> { for (int i = 0; i < ITERS; i++) { a.updateAndGet(v -> v + 1); } });
            return a.get() + "/" + total;
        });
        t("aia.perSlot.total", () -> {
            AtomicIntegerArray arr = new AtomicIntegerArray(8);
            race(() -> { for (int i = 0; i < ITERS; i++) { arr.getAndIncrement(i % 8); } });
            int sum = 0;
            for (int i = 0; i < 8; i++) { sum += arr.get(i); }
            return sum + "/" + total;
        });

        // ---- a CAS race has exactly one winner ------------------------------
        t("ai.cas.oneWinner", () -> {
            AtomicInteger a = new AtomicInteger(0);
            AtomicInteger winners = new AtomicInteger();
            race(() -> { if (a.compareAndSet(0, 1)) { winners.incrementAndGet(); } });
            return winners.get() + " winner(s), value=" + a.get();
        });
        t("ar.cas.oneWinner", () -> {
            AtomicReference<String> r = new AtomicReference<>(null);
            AtomicInteger winners = new AtomicInteger();
            race(() -> { if (r.compareAndSet(null, "x")) { winners.incrementAndGet(); } });
            return winners.get() + " winner(s), value=" + r.get();
        });

        // ---- locks must serialise a NON-atomic read-modify-write ------------
        t("reentrantLock.serialises", () -> {
            final int[] plain = new int[1];
            java.util.concurrent.locks.ReentrantLock lock =
                    new java.util.concurrent.locks.ReentrantLock();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                lock.lock();
                try { plain[0]++; } finally { lock.unlock(); }
            } });
            return plain[0] + "/" + total;
        });
        t("synchronized.serialises", () -> {
            final int[] plain = new int[1];
            final Object mon = new Object();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                synchronized (mon) { plain[0]++; }
            } });
            return plain[0] + "/" + total;
        });
        t("rwLock.writeSerialises", () -> {
            final int[] plain = new int[1];
            java.util.concurrent.locks.ReentrantReadWriteLock lock =
                    new java.util.concurrent.locks.ReentrantReadWriteLock();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                lock.writeLock().lock();
                try { plain[0]++; } finally { lock.writeLock().unlock(); }
            } });
            return plain[0] + "/" + total;
        });

        // ---- ConcurrentHashMap: this lane's flagship, under contention ------
        t("chm.merge.total", () -> {
            ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
            race(() -> { for (int i = 0; i < ITERS; i++) { m.merge("k", 1, Integer::sum); } });
            return m.get("k") + "/" + total;
        });
        t("chm.compute.total", () -> {
            ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                m.compute("k", (k, v) -> v == null ? 1 : v + 1); } });
            return m.get("k") + "/" + total;
        });
        t("chm.distinctKeys.size", () -> {
            ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
            AtomicInteger seq = new AtomicInteger();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                int k = seq.getAndIncrement(); m.put(k, k); } });
            return m.size() + "/" + total;
        });
        t("chm.putIfAbsent.oneWinner", () -> {
            ConcurrentHashMap<String, String> m = new ConcurrentHashMap<>();
            AtomicInteger winners = new AtomicInteger();
            race(() -> { if (m.putIfAbsent("k", Thread.currentThread().getName()) == null) {
                winners.incrementAndGet(); } });
            return winners.get() + " winner(s), size=" + m.size();
        });
        t("chm.computeIfAbsent.once", () -> {
            ConcurrentHashMap<String, Integer> m = new ConcurrentHashMap<>();
            AtomicInteger calls = new AtomicInteger();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                m.computeIfAbsent("k", k -> calls.incrementAndGet()); } });
            return "mapping=" + m.get("k") + " calls=" + calls.get();
        });
        t("chm.remove.conservation", () -> {
            ConcurrentHashMap<Integer, Integer> m = new ConcurrentHashMap<>();
            for (int i = 0; i < total; i++) { m.put(i, i); }
            AtomicInteger removed = new AtomicInteger();
            AtomicInteger seq = new AtomicInteger();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                if (m.remove(seq.getAndIncrement()) != null) { removed.incrementAndGet(); } } });
            return removed.get() + " removed, " + m.size() + " left";
        });

        // ---- queues: what goes in comes out, exactly once -------------------
        t("clq.offerDrain.conservation", () -> {
            ConcurrentLinkedQueue<Integer> q = new ConcurrentLinkedQueue<>();
            race(() -> { for (int i = 0; i < ITERS; i++) { q.offer(i); } });
            int drained = 0;
            while (q.poll() != null) { drained++; }
            return drained + "/" + total;
        });
        t("lbq.putTake.conservation", () -> {
            LinkedBlockingQueue<Integer> q = new LinkedBlockingQueue<>();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                try { q.put(i); } catch (InterruptedException ignored) { } } });
            List<Integer> out = new ArrayList<>();
            q.drainTo(out);
            return out.size() + "/" + total;
        });
        t("abq.boundedRoundTrip", () -> {
            ArrayBlockingQueue<Integer> q = new ArrayBlockingQueue<>(64);
            AtomicInteger taken = new AtomicInteger();
            Thread consumer = new Thread(() -> {
                try { for (int i = 0; i < total; i++) { q.take(); taken.incrementAndGet(); } }
                catch (InterruptedException ignored) { }
            });
            consumer.setDaemon(true);
            consumer.start();
            race(() -> { for (int i = 0; i < ITERS; i++) {
                try { q.put(i); } catch (InterruptedException ignored) { } } });
            consumer.join(30000);
            return taken.get() + "/" + total;
        });
        t("cowl.add.size", () -> {
            CopyOnWriteArrayList<Integer> l = new CopyOnWriteArrayList<>();
            race(() -> { for (int i = 0; i < 200; i++) { l.add(i); } });
            return l.size() + "/" + (THREADS * 200);
        });

        // ---- the synchronizers actually synchronising -----------------------
        t("cdl.allCountDown.releases", () -> {
            CountDownLatch l = new CountDownLatch(THREADS);
            AtomicInteger before = new AtomicInteger();
            Thread waiter = new Thread(() -> {
                try { l.await(); before.set((int) l.getCount()); } catch (InterruptedException ignored) { }
            });
            waiter.setDaemon(true);
            waiter.start();
            race(l::countDown);
            waiter.join(30000);
            return "count=" + l.getCount() + " seenAtRelease=" + before.get();
        });
        t("sem.permitsConserved", () -> {
            Semaphore s = new Semaphore(2);
            race(() -> { for (int i = 0; i < ITERS; i++) {
                try { s.acquire(); } catch (InterruptedException ignored) { }
                s.release();
            } });
            return s.availablePermits() + "/2";
        });
        t("cb.tripsExactlyOnce", () -> {
            AtomicInteger trips = new AtomicInteger();
            CyclicBarrier b = new CyclicBarrier(THREADS, trips::incrementAndGet);
            race(() -> { try { b.await(20, TimeUnit.SECONDS); }
                catch (Exception ignored) { } });
            return "trips=" + trips.get() + " broken=" + b.isBroken();
        });
        t("ph.arriveAndAwait.phase", () -> {
            Phaser ph = new Phaser(THREADS);
            race(ph::arriveAndAwaitAdvance);
            return "phase=" + ph.getPhase() + " registered=" + ph.getRegisteredParties();
        });

        // ---- executor: every submitted task runs exactly once ---------------
        t("pool.everyTaskRuns", () -> {
            ExecutorService ex = Executors.newFixedThreadPool(THREADS);
            AtomicInteger ran = new AtomicInteger();
            try {
                List<Future<?>> fs = new ArrayList<>();
                for (int i = 0; i < 2000; i++) { fs.add(ex.submit(ran::incrementAndGet)); }
                for (Future<?> f : fs) { f.get(30, TimeUnit.SECONDS); }
            } finally { ex.shutdownNow(); }
            return ran.get() + "/2000";
        });
        t("pool.invokeAll.allDone", () -> {
            ExecutorService ex = Executors.newFixedThreadPool(THREADS);
            try {
                List<Callable<Integer>> cs = new ArrayList<>();
                for (int i = 0; i < 500; i++) { final int v = i; cs.add(() -> v); }
                List<Future<Integer>> fs = ex.invokeAll(cs);
                int sum = 0;
                boolean allDone = true;
                for (Future<Integer> f : fs) { sum += f.get(); allDone &= f.isDone(); }
                return "sum=" + sum + " allDone=" + allDone;
            } finally { ex.shutdownNow(); }
        });

        System.out.println("SWEEP-DONE");
    }
}
