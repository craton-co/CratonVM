import java.util.concurrent.*;
import java.util.concurrent.locks.*;

/**
 * L6 differential sweep: the java.util.concurrent SYNCHRONIZER family, which
 * this lane's first pass did not reach (it took ConcurrentHashMap, Thread,
 * ForkJoinTask/Pool and AsynchronousFileChannel).
 *
 * Weighted at the ARGUMENT-VALIDATION and REFUSAL surface, where this campaign
 * keeps finding defects: a JDK method is a bytecode wrapper of checks over a
 * native that does the work, so the checks are what a native shadow drops.
 *
 * One row per assertion; never a nested print inside a row, so a VM that throws
 * where the other answers still costs exactly one diff line.
 */
public class SyncFamilySweep {
    /**
     * Drop `@<hex>` identity hashes so a row compares across VMs.
     *
     * Hand-rolled rather than `String.replaceAll`: a probe must not normalise
     * itself with machinery the VM under test also implements, or a regex
     * defect reads as agreement. charAt/StringBuilder only.
     */
    static String norm(String s) {
        StringBuilder b = new StringBuilder(s.length());
        int i = 0;
        while (i < s.length()) {
            char c = s.charAt(i);
            b.append(c);
            i++;
            if (c != '@') {
                continue;
            }
            int j = i;
            if (j + 1 < s.length() && s.charAt(j) == '0' && s.charAt(j + 1) == 'x') {
                j += 2;
            }
            int start = j;
            while (j < s.length()) {
                char h = s.charAt(j);
                boolean hex = (h >= '0' && h <= '9') || (h >= 'a' && h <= 'f') || (h >= 'A' && h <= 'F');
                if (!hex) {
                    break;
                }
                j++;
            }
            if (j > start) {
                b.append("<id>");
                i = j;
            }
        }
        return b.toString();
    }

    static void p(String tag, Object v) { System.out.println(tag + " |" + norm(String.valueOf(v)) + "|"); }

    /** Value-or-throwable, one row either way. */
    static void t(String tag, Callable<Object> c) {
        try { p(tag, c.call()); }
        catch (Throwable e) {
            String m = e.getMessage();
            p(tag, "THREW " + e.getClass().getName() + (m == null ? "" : ": " + m));
        }
    }

    /** t() with a hard deadline, so a hang is a ROW rather than a dead sweep. */
    static void tw(String tag, Callable<Object> c) {
        final Object[] out = new Object[1];
        Thread th = new Thread(() -> {
            try { out[0] = c.call(); }
            catch (Throwable e) {
                String m = e.getMessage();
                out[0] = "THREW " + e.getClass().getName() + (m == null ? "" : ": " + m);
            }
        });
        th.setDaemon(true);
        th.start();
        try { th.join(5000); } catch (InterruptedException ignored) { }
        p(tag, th.isAlive() ? "TIMEOUT-5s" : String.valueOf(out[0]));
    }

    public static void main(String[] a) throws Exception {
        // ---- CountDownLatch ------------------------------------------------
        t("cdl.ctor(-1)", () -> new CountDownLatch(-1));
        t("cdl.ctor(0).getCount", () -> new CountDownLatch(0).getCount());
        t("cdl.ctor(2).getCount", () -> new CountDownLatch(2).getCount());
        t("cdl.countDown.below0", () -> { CountDownLatch l = new CountDownLatch(1);
            l.countDown(); l.countDown(); return l.getCount(); });
        tw("cdl.await(0,NANOS)", () -> new CountDownLatch(1).await(0, TimeUnit.NANOSECONDS));
        t("cdl.await(null unit)", () -> new CountDownLatch(1).await(1, null));
        t("cdl.toString", () -> new CountDownLatch(3).toString());

        // ---- Semaphore -----------------------------------------------------
        t("sem.ctor(-2).permits", () -> new Semaphore(-2).availablePermits());
        t("sem.acquire(-1)", () -> { new Semaphore(1).acquire(-1); return "no-throw"; });
        t("sem.release(-1)", () -> { new Semaphore(1).release(-1); return "no-throw"; });
        t("sem.tryAcquire(-1)", () -> new Semaphore(1).tryAcquire(-1));
        t("sem.drainPermits", () -> new Semaphore(4).drainPermits());
        t("sem.isFair.default", () -> new Semaphore(1).isFair());
        t("sem.toString", () -> new Semaphore(2).toString());
        t("sem.tryAcquire(null unit)", () -> new Semaphore(1).tryAcquire(1, null));

        // ---- CyclicBarrier -------------------------------------------------
        t("cb.ctor(0)", () -> new CyclicBarrier(0));
        t("cb.ctor(-1)", () -> new CyclicBarrier(-1));
        t("cb.getParties", () -> new CyclicBarrier(3).getParties());
        t("cb.getNumberWaiting", () -> new CyclicBarrier(3).getNumberWaiting());
        t("cb.isBroken", () -> new CyclicBarrier(3).isBroken());
        tw("cb.await(1,NANOS)", () -> new CyclicBarrier(2).await(1, TimeUnit.NANOSECONDS));
        t("cb.reset.then.isBroken", () -> { CyclicBarrier b = new CyclicBarrier(2);
            b.reset(); return b.isBroken(); });
        tw("cb.single.await", () -> new CyclicBarrier(1).await());

        // ---- Exchanger -----------------------------------------------------
        tw("exch.timeout", () -> new Exchanger<String>().exchange("x", 1, TimeUnit.MILLISECONDS));

        // ---- Phaser --------------------------------------------------------
        t("ph.ctor(-1)", () -> new Phaser(-1));
        t("ph.getPhase", () -> new Phaser().getPhase());
        t("ph.register.parties", () -> { Phaser p = new Phaser(); p.register(); return p.getRegisteredParties(); });
        t("ph.bulkRegister(-1)", () -> new Phaser().bulkRegister(-1));
        t("ph.arrive.unregistered", () -> new Phaser().arrive());
        t("ph.arriveAndDeregister.un", () -> new Phaser().arriveAndDeregister());
        t("ph.isTerminated", () -> new Phaser().isTerminated());
        t("ph.getUnarrivedParties", () -> new Phaser(2).getUnarrivedParties());
        t("ph.toString", () -> new Phaser(1).toString());

        // ---- ReentrantLock -------------------------------------------------
        t("rl.unlock.notOwner", () -> { new ReentrantLock().unlock(); return "no-throw"; });
        t("rl.isHeldByCurrentThread", () -> new ReentrantLock().isHeldByCurrentThread());
        t("rl.getHoldCount.0", () -> new ReentrantLock().getHoldCount());
        t("rl.lock.holdCount", () -> { ReentrantLock l = new ReentrantLock();
            l.lock(); l.lock(); int h = l.getHoldCount(); l.unlock(); l.unlock(); return h; });
        t("rl.tryLock(null unit)", () -> new ReentrantLock().tryLock(1, null));
        t("rl.condition.signal.un", () -> { ReentrantLock l = new ReentrantLock();
            l.newCondition().signal(); return "no-throw"; });
        t("rl.isFair.default", () -> new ReentrantLock().isFair());
        t("rl.isLocked", () -> new ReentrantLock().isLocked());
        t("rl.toString.unlocked", () -> new ReentrantLock().toString());

        // ---- ReentrantReadWriteLock ---------------------------------------
        t("rw.readUnlock.notHeld", () -> { new ReentrantReadWriteLock().readLock().unlock(); return "no-throw"; });
        t("rw.writeUnlock.notHeld", () -> { new ReentrantReadWriteLock().writeLock().unlock(); return "no-throw"; });
        t("rw.getReadHoldCount", () -> new ReentrantReadWriteLock().getReadHoldCount());
        t("rw.isWriteLocked", () -> new ReentrantReadWriteLock().isWriteLocked());
        t("rw.writeThenRead", () -> { ReentrantReadWriteLock l = new ReentrantReadWriteLock();
            l.writeLock().lock(); boolean ok = l.readLock().tryLock();
            if (ok) l.readLock().unlock(); l.writeLock().unlock(); return ok; });
        t("rw.readThenWrite", () -> { ReentrantReadWriteLock l = new ReentrantReadWriteLock();
            l.readLock().lock(); boolean ok = l.writeLock().tryLock();
            if (ok) l.writeLock().unlock(); l.readLock().unlock(); return ok; });

        // ---- StampedLock ---------------------------------------------------
        t("sl.tryOptimisticRead.nz", () -> new StampedLock().tryOptimisticRead() != 0L);
        t("sl.validate(0)", () -> new StampedLock().validate(0L));
        t("sl.unlockWrite(bad)", () -> { new StampedLock().unlockWrite(1L); return "no-throw"; });
        t("sl.unlockRead(bad)", () -> { new StampedLock().unlockRead(1L); return "no-throw"; });
        t("sl.unlock(bad)", () -> { new StampedLock().unlock(1L); return "no-throw"; });
        t("sl.tryUnlockWrite.un", () -> new StampedLock().tryUnlockWrite());
        t("sl.tryUnlockRead.un", () -> new StampedLock().tryUnlockRead());
        t("sl.isWriteLocked", () -> new StampedLock().isWriteLocked());

        // ---- CompletableFuture --------------------------------------------
        t("cf.completedFuture.get", () -> CompletableFuture.completedFuture(7).get());
        t("cf.failed.get", () -> CompletableFuture.failedFuture(new IllegalStateException("boom")).get());
        t("cf.getNow.default", () -> new CompletableFuture<Integer>().getNow(42));
        t("cf.complete.twice", () -> { CompletableFuture<Integer> f = new CompletableFuture<>();
            f.complete(1); return f.complete(2); });
        t("cf.cancel.isCancelled", () -> { CompletableFuture<Integer> f = new CompletableFuture<>();
            f.cancel(true); return f.isCancelled(); });
        t("cf.join.cancelled", () -> { CompletableFuture<Integer> f = new CompletableFuture<>();
            f.cancel(true); return f.join(); });
        t("cf.thenApply.null", () -> CompletableFuture.completedFuture(1).thenApply(null));
        t("cf.allOf.empty.get", () -> CompletableFuture.allOf().get());
        t("cf.anyOf.empty.isDone", () -> CompletableFuture.anyOf().isDone());
        t("cf.get(0,null)", () -> new CompletableFuture<Integer>().get(0, null));
        t("cf.isCompletedExc", () -> CompletableFuture.failedFuture(new RuntimeException()).isCompletedExceptionally());

        // ---- Executors / ThreadPoolExecutor --------------------------------
        t("tpe.ctor.negCore", () -> new ThreadPoolExecutor(-1, 1, 0, TimeUnit.SECONDS, new LinkedBlockingQueue<>()));
        t("tpe.ctor.maxLtCore", () -> new ThreadPoolExecutor(2, 1, 0, TimeUnit.SECONDS, new LinkedBlockingQueue<>()));
        t("tpe.ctor.negKeepAlive", () -> new ThreadPoolExecutor(1, 1, -1, TimeUnit.SECONDS, new LinkedBlockingQueue<>()));
        t("tpe.ctor.nullQueue", () -> new ThreadPoolExecutor(1, 1, 0, TimeUnit.SECONDS, null));
        t("exec.newFixed(0)", () -> Executors.newFixedThreadPool(0));
        t("exec.newFixed(-1)", () -> Executors.newFixedThreadPool(-1));
        t("exec.callable(null)", () -> Executors.callable((Runnable) null));
        t("tpe.execute.null", () -> { ExecutorService e = Executors.newFixedThreadPool(1);
            try { e.execute(null); return "no-throw"; } finally { e.shutdownNow(); } });
        t("tpe.submit.null", () -> { ExecutorService e = Executors.newFixedThreadPool(1);
            try { return e.submit((Runnable) null); } finally { e.shutdownNow(); } });
        // CLASS ONLY. The message embeds the submitted lambda's class name, which
        // carries a per-run address (`$$Lambda/0x00000000a104b810`) that HotSpot
        // varies between its own runs — measured, 2 of 3 oracle runs disagreed.
        // A row the oracle cannot answer twice is not a contract.
        t("tpe.afterShutdown.class", () -> { ExecutorService e = Executors.newFixedThreadPool(1);
            e.shutdown();
            try { e.submit(() -> 1); return "no-throw"; }
            catch (Throwable ex) { return ex.getClass().getName(); }
            finally { e.shutdownNow(); } });
        t("tpe.isShutdown", () -> { ExecutorService e = Executors.newFixedThreadPool(1);
            e.shutdown(); return e.isShutdown(); });
        t("tpe.awaitTerm(null)", () -> { ExecutorService e = Executors.newFixedThreadPool(1);
            e.shutdown(); try { return e.awaitTermination(1, null); } finally { e.shutdownNow(); } });

        // ---- ScheduledThreadPoolExecutor ----------------------------------
        t("stpe.schedule.null.unit", () -> { ScheduledExecutorService e = Executors.newScheduledThreadPool(1);
            try { return e.schedule(() -> 1, 1, null); } finally { e.shutdownNow(); } });
        t("stpe.fixedRate.0", () -> { ScheduledExecutorService e = Executors.newScheduledThreadPool(1);
            try { return e.scheduleAtFixedRate(() -> { }, 0, 0, TimeUnit.SECONDS); } finally { e.shutdownNow(); } });

        // ---- TimeUnit ------------------------------------------------------
        t("tu.convert.overflow", () -> TimeUnit.DAYS.toNanos(Long.MAX_VALUE));
        t("tu.valueOf.bad", () -> TimeUnit.valueOf("FORTNIGHT"));
        t("tu.sleep.negative", () -> { TimeUnit.NANOSECONDS.sleep(-1); return "no-throw"; });
        t("tu.toString", () -> TimeUnit.MILLISECONDS.toString());

        System.out.println("SWEEP-DONE");
    }
}
