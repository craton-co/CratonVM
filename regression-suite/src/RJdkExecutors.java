import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.CancellationException;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionException;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.RejectedExecutionException;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.TimeoutException;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

/**
 * JDK-only corpus: executors -- fixed pool, scheduled executor, shutdown,
 * interruption.
 *
 * Complements RExecutorShutdown (which pins the orderly-vs-forceful shutdown
 * contract). This vector covers the surrounding API: submission, futures,
 * cancellation, invokeAll/invokeAny, thread factories, rejection policies and
 * the scheduled executor.
 *
 * "Thread and executor semantics" is a named P1 blocker: CratonVM's
 * invoke_or_native special-cases ThreadPoolExecutor.execute on the SHAPE of the
 * receiver, so real bytecode does not always run.
 *
 * Determinism: no elapsed-time assertions and no thread names or ids are
 * printed; every rendezvous is a latch, and every emitted collection is sorted
 * or comes back in submission order.
 */
public class RJdkExecutors {
    static final long T = 30;   // generous seconds for any bounded wait
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void fixedPool() throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(4);
        try {
            // Futures come back in submission order regardless of completion order.
            List<Future<Integer>> fs = new ArrayList<>();
            for (int i = 0; i < 40; i++) {
                final int v = i;
                fs.add(ex.submit(() -> v * v));
            }
            long sum = 0;
            for (Future<Integer> f : fs) {
                sum += f.get(T, TimeUnit.SECONDS);
            }
            check(sum == 20540, "sum of squares 0..39: " + sum);
            check(fs.get(3).isDone(), "isDone after get");
            check(!fs.get(3).isCancelled(), "not cancelled");

            // A Runnable submission with a fixed result.
            Future<String> r = ex.submit(() -> { }, "done");
            check("done".equals(r.get(T, TimeUnit.SECONDS)), "submit(Runnable, result)");

            // A task exception surfaces as ExecutionException with the real cause.
            Future<Integer> bad = ex.submit(() -> {
                throw new IllegalStateException("task-boom");
            });
            boolean threw = false;
            try {
                bad.get(T, TimeUnit.SECONDS);
            } catch (ExecutionException e) {
                threw = e.getCause() instanceof IllegalStateException
                        && "task-boom".equals(e.getCause().getMessage());
            }
            check(threw, "a task exception must arrive as ExecutionException(cause)");
            check(bad.isDone() && !bad.isCancelled(), "failed task is done, not cancelled");

            // get() with a timeout on a task that has not finished.
            CountDownLatch hold = new CountDownLatch(1);
            Future<?> slow = ex.submit(() -> {
                hold.await();
                return 1;
            });
            threw = false;
            try {
                slow.get(50, TimeUnit.MILLISECONDS);
            } catch (TimeoutException expected) {
                threw = true;
            }
            check(threw, "get() must time out while the task is still running");
            hold.countDown();
            check(((Integer) slow.get(T, TimeUnit.SECONDS)) == 1, "task completes after release");

            // Cancellation before the task can start.
            CountDownLatch block = new CountDownLatch(1);
            CountDownLatch started = new CountDownLatch(4);
            ExecutorService single = Executors.newFixedThreadPool(1);
            single.submit(() -> {
                block.await();
                return 0;
            });
            Future<?> queued = single.submit(() -> 1);
            check(queued.cancel(false), "a queued task must be cancellable");
            check(queued.isCancelled() && queued.isDone(), "cancelled state");
            threw = false;
            try {
                queued.get();
            } catch (CancellationException expected) {
                threw = true;
            }
            check(threw, "get() on a cancelled task must throw CancellationException");
            block.countDown();
            single.shutdown();
            check(single.awaitTermination(T, TimeUnit.SECONDS), "single pool terminated");
            check(started.getCount() == 4, "unused latch untouched");

            // invokeAll returns one future per task, in argument order, all done.
            List<Callable<String>> batch = new ArrayList<>();
            for (int i = 0; i < 6; i++) {
                final int v = i;
                batch.add(() -> "t" + v);
            }
            List<Future<String>> all = ex.invokeAll(batch);
            List<String> got = new ArrayList<>();
            for (Future<String> f : all) {
                check(f.isDone(), "invokeAll must return completed futures");
                got.add(f.get());
            }
            check(got.equals(Arrays.asList("t0", "t1", "t2", "t3", "t4", "t5")),
                    "invokeAll order: " + got);

            // invokeAny returns SOME successful result -- which one is unspecified,
            // so only membership is asserted.
            String any = ex.invokeAny(batch);
            check(got.contains(any), "invokeAny result must be one of the tasks: " + any);
            System.out.println("CK RJdkExecutors fixed sum=" + sum + " invokeAll=" + got);
        } finally {
            ex.shutdown();
            check(ex.awaitTermination(T, TimeUnit.SECONDS), "fixed pool terminated");
        }
    }

    static void factoryAndRejection() throws Exception {
        AtomicInteger created = new AtomicInteger();
        ThreadFactory tf = r -> {
            created.incrementAndGet();
            Thread t = new Thread(r, "rjdk-worker-" + created.get());
            t.setDaemon(true);
            return t;
        };
        CountDownLatch block = new CountDownLatch(1);
        CountDownLatch running = new CountDownLatch(1);
        ThreadPoolExecutor tpe = new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS,
                new LinkedBlockingQueue<>(1), tf, new ThreadPoolExecutor.AbortPolicy());
        try {
            tpe.execute(() -> {
                running.countDown();
                try {
                    block.await();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            });
            check(running.await(T, TimeUnit.SECONDS), "worker never started");
            check(created.get() >= 1, "the custom ThreadFactory must be used");
            tpe.execute(() -> { });                       // fills the 1-slot queue
            boolean threw = false;
            try {
                tpe.execute(() -> { });                   // over capacity -> abort
            } catch (RejectedExecutionException expected) {
                threw = true;
            }
            check(threw, "AbortPolicy must reject when core+queue are full");
            check(tpe.getCorePoolSize() == 1 && tpe.getMaximumPoolSize() == 1, "pool sizing");
            block.countDown();
        } finally {
            tpe.shutdown();
            check(tpe.awaitTermination(T, TimeUnit.SECONDS), "tpe terminated");
        }
        check(tpe.isShutdown() && tpe.isTerminated(), "terminal state flags");

        // CallerRunsPolicy executes on the submitting thread when saturated.
        final String main = Thread.currentThread().getName();
        final List<String> ranOn = Collections.synchronizedList(new ArrayList<>());
        CountDownLatch hold = new CountDownLatch(1);
        CountDownLatch up = new CountDownLatch(1);
        ThreadPoolExecutor cr = new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS,
                new LinkedBlockingQueue<>(1), new ThreadPoolExecutor.CallerRunsPolicy());
        try {
            cr.execute(() -> {
                up.countDown();
                try {
                    hold.await();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                }
            });
            check(up.await(T, TimeUnit.SECONDS), "caller-runs worker never started");
            cr.execute(() -> { });
            cr.execute(() -> ranOn.add(Thread.currentThread().getName()));
            check(ranOn.size() == 1 && main.equals(ranOn.get(0)),
                    "CallerRunsPolicy must run the task on the submitting thread");
            hold.countDown();
        } finally {
            cr.shutdown();
            check(cr.awaitTermination(T, TimeUnit.SECONDS), "caller-runs pool terminated");
        }
        System.out.println("CK RJdkExecutors rejection ok callerRuns=" + (ranOn.size() == 1));
    }

    static void scheduled() throws Exception {
        ScheduledExecutorService ses = Executors.newScheduledThreadPool(2);
        try {
            ScheduledFuture<String> f = ses.schedule(() -> "later", 20, TimeUnit.MILLISECONDS);
            check("later".equals(f.get(T, TimeUnit.SECONDS)), "schedule(Callable)");
            check(f.isDone(), "scheduled future done");

            // Ordering, not timing: a longer delay must complete after a shorter
            // one, which we observe through the recorded completion sequence.
            final List<String> order = Collections.synchronizedList(new ArrayList<>());
            CountDownLatch both = new CountDownLatch(2);
            ses.schedule(() -> {
                order.add("late");
                both.countDown();
            }, 300, TimeUnit.MILLISECONDS);
            ses.schedule(() -> {
                order.add("early");
                both.countDown();
            }, 20, TimeUnit.MILLISECONDS);
            check(both.await(T, TimeUnit.SECONDS), "both scheduled tasks ran");
            check(order.equals(Arrays.asList("early", "late")),
                    "scheduled ordering must follow delay: " + order);

            // A fixed-rate task must be cancellable and must stop firing.
            AtomicInteger ticks = new AtomicInteger();
            CountDownLatch five = new CountDownLatch(5);
            ScheduledFuture<?> rate = ses.scheduleAtFixedRate(() -> {
                ticks.incrementAndGet();
                five.countDown();
            }, 0, 5, TimeUnit.MILLISECONDS);
            check(five.await(T, TimeUnit.SECONDS), "fixed-rate task never reached 5 ticks");
            check(rate.cancel(false), "cancel a periodic task");
            check(rate.isCancelled(), "periodic task cancelled");
            Thread.sleep(100);
            int after = ticks.get();
            Thread.sleep(100);
            check(ticks.get() == after, "a cancelled periodic task must stop firing");

            // scheduleWithFixedDelay, same shape.
            CountDownLatch three = new CountDownLatch(3);
            ScheduledFuture<?> delay = ses.scheduleWithFixedDelay(three::countDown,
                    0, 5, TimeUnit.MILLISECONDS);
            check(three.await(T, TimeUnit.SECONDS), "fixed-delay task never reached 3 ticks");
            delay.cancel(false);
            System.out.println("CK RJdkExecutors scheduled order=" + order + " ticksStable=true");
        } finally {
            ses.shutdownNow();
            check(ses.awaitTermination(T, TimeUnit.SECONDS), "scheduled pool terminated");
        }
    }

    /** A blocked task must observe the interrupt that shutdownNow delivers. */
    static void interruption() throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(2);
        CountDownLatch started = new CountDownLatch(1);
        CountDownLatch interrupted = new CountDownLatch(1);
        final List<String> observed = Collections.synchronizedList(new ArrayList<>());
        ex.submit(() -> {
            started.countDown();
            try {
                Thread.sleep(T * 1000);
                observed.add("slept");
            } catch (InterruptedException e) {
                observed.add("interrupted");
                // The JDK clears the flag when InterruptedException is thrown.
                observed.add("flagCleared=" + !Thread.currentThread().isInterrupted());
                interrupted.countDown();
            }
        });
        check(started.await(T, TimeUnit.SECONDS), "task never started");
        List<Runnable> pending = ex.shutdownNow();
        check(pending.isEmpty(), "no task should still be queued: " + pending.size());
        check(interrupted.await(T, TimeUnit.SECONDS), "shutdownNow did not interrupt the task");
        check(ex.awaitTermination(T, TimeUnit.SECONDS), "pool terminated after shutdownNow");
        check(observed.equals(Arrays.asList("interrupted", "flagCleared=true")),
                "interruption record: " + observed);

        // Submitting to a shut-down executor is rejected.
        boolean threw = false;
        try {
            ex.submit(() -> 1);
        } catch (RejectedExecutionException expected) {
            threw = true;
        }
        check(threw, "submission after shutdown must be rejected");
        System.out.println("CK RJdkExecutors interruption=" + observed);
    }

    static void completableFutures() throws Exception {
        ExecutorService ex = Executors.newFixedThreadPool(3);
        try {
            CompletableFuture<Integer> a = CompletableFuture.supplyAsync(() -> 20, ex);
            CompletableFuture<Integer> b = CompletableFuture.supplyAsync(() -> 22, ex);
            check(a.thenCombine(b, Integer::sum).get(T, TimeUnit.SECONDS) == 42, "thenCombine");
            check(a.thenApply(x -> x * 2).get(T, TimeUnit.SECONDS) == 40, "thenApply");
            check(a.thenCompose(x -> CompletableFuture.completedFuture(x + 1))
                    .get(T, TimeUnit.SECONDS) == 21, "thenCompose");
            CompletableFuture<Void> all = CompletableFuture.allOf(a, b);
            all.get(T, TimeUnit.SECONDS);
            check(all.isDone(), "allOf");

            CompletableFuture<Integer> boom = CompletableFuture.supplyAsync(() -> {
                throw new IllegalArgumentException("cf-boom");
            }, ex);
            boolean threw = false;
            try {
                boom.join();
            } catch (CompletionException e) {
                threw = e.getCause() instanceof IllegalArgumentException;
            }
            check(threw, "join() must wrap the cause in CompletionException");
            check(boom.isCompletedExceptionally(), "isCompletedExceptionally");
            check(boom.exceptionally(t -> -1).get(T, TimeUnit.SECONDS) == -1, "exceptionally");
            check(boom.handle((v, t) -> t == null ? 0 : 1).get(T, TimeUnit.SECONDS) == 1, "handle");
            System.out.println("CK RJdkExecutors completable=42");
        } finally {
            ex.shutdown();
            check(ex.awaitTermination(T, TimeUnit.SECONDS), "cf pool terminated");
        }
    }

    /**
     * The VM must give a terminating thread its Java-side cleanup --
     * `Thread.exit()` -- and it must do it on the ABNORMAL path too.
     *
     * Why this is assertable from public API at all. `Thread.exit()` ends in
     * `clearReferences()`, which nulls the per-instance
     * `uncaughtExceptionHandler` field. `getUncaughtExceptionHandler()` reads
     * that field and falls back to the ThreadGroup when it is null, so a
     * terminated thread that still hands back the handler somebody installed on
     * it is a thread whose `exit()` never ran. Nothing here needs
     * `--add-opens`, reflection, `StructuredTaskScope` or a ThreadFlock.
     *
     * Why the ABNORMAL path and not the normal one, which is the whole point.
     * A cleanup wired only into the happy path is the specific defect to look
     * for, and it is also the path that matters: a task that ends by throwing
     * is exactly when a structured-concurrency owner is parked waiting for a
     * container count to fall. Two assertions, in this order, because the
     * second is meaningless without the first: the handler must FIRE (proving
     * the uncaught dispatch consulted it while it was still installed), and
     * only then must it be GONE.
     *
     * Why the normal-return thread is deliberately NOT asserted the same way.
     * CratonVM keeps per-thread handlers in a side table as well as in the real
     * field, and the entry is removed only by the uncaught dispatch -- so after
     * a CLEAN death the side table still holds it, and whether
     * `getUncaughtExceptionHandler()` sees the side table or the (nulled) real
     * field depends on which of the two wins dispatch, which differs per mode.
     * Asserting it here would encode a mode-dependent answer. The clean thread
     * is still started and joined below, so the section covers both paths for
     * everything that IS mode-independent.
     */
    static void threadExitCleanup() throws Exception {
        // --- abnormal termination: run() throws ---
        final AtomicInteger fired = new AtomicInteger();
        final AtomicReference<String> caught = new AtomicReference<>("none");
        Thread.UncaughtExceptionHandler ueh = (t, e) -> {
            fired.incrementAndGet();
            caught.set(e.getClass().getName() + ":" + e.getMessage());
        };
        Thread bad = new Thread(() -> {
            throw new IllegalStateException("exit-boom");
        }, "rjdk-exit-abnormal");
        bad.setUncaughtExceptionHandler(ueh);
        check(bad.getUncaughtExceptionHandler() == ueh,
                "a live thread must report the handler that was installed on it");
        bad.start();
        bad.join(T * 1000);
        check(!bad.isAlive(), "the throwing thread must have terminated");
        // `>= 1`, not `== 1`, and the looseness is deliberate rather than lazy:
        // CratonVM dispatches through a side table AND lets the real
        // `Thread.dispatchUncaughtException` bytecode run on some paths, so the
        // exact count is a per-mode fact. What this assertion needs is only that
        // the handler was consulted WHILE STILL INSTALLED, which is what makes
        // the "and now it is gone" check below mean something. The exact-count
        // question is asked below instead, as a DELTA across the clean thread,
        // where it is mode-independent.
        int firedAfterAbnormal = fired.get();
        check(firedAfterAbnormal >= 1,
                "the per-thread handler must fire on an uncaught exception: " + firedAfterAbnormal);
        check("java.lang.IllegalStateException:exit-boom".equals(caught.get()),
                "the handler must receive the original throwable: " + caught.get());
        // THE discriminator. Before Thread.exit() was wired up this answered the
        // handler itself, on both VMs' abnormal path; HotSpot answers the
        // ThreadGroup (or null for a terminated thread), never the handler.
        check(bad.getUncaughtExceptionHandler() != ueh,
                "Thread.exit() must clear a terminated thread's uncaught handler; got "
                        + bad.getUncaughtExceptionHandler());

        // --- normal termination: the clean path must still not be disturbed ---
        final AtomicInteger ran = new AtomicInteger();
        Thread ok = new Thread(ran::incrementAndGet, "rjdk-exit-normal");
        ok.setUncaughtExceptionHandler(ueh);
        ok.start();
        ok.join(T * 1000);
        check(!ok.isAlive(), "the clean thread must have terminated");
        check(ran.get() == 1, "the clean thread's body must have run once: " + ran.get());
        check(fired.get() == firedAfterAbnormal,
                "a clean exit must not dispatch an uncaught handler: " + fired.get()
                        + " != " + firedAfterAbnormal);
        // A terminated thread is not restartable, and this is asserted here
        // rather than anywhere else because running Java teardown on a dying
        // thread is exactly the change that could plausibly resurrect one.
        boolean threw = false;
        try {
            ok.start();
        } catch (IllegalThreadStateException expected) {
            threw = true;
        }
        check(threw, "restarting a terminated thread must throw IllegalThreadStateException");
        // The exception is the SYMPTOM; this is the damage. Measured on
        // CratonVM before the fix: the second `start()` did not merely fail to
        // throw, it RE-RAN the body, so `ran` went 1 -> 2 and a Runnable the
        // application had already retired executed a second time on a second
        // OS thread. A guard that throws but still spawns would pass the check
        // above and fail this one. `join` rather than a sleep: if the thread
        // was wrongly resurrected this waits for that second run to finish, and
        // if `start()` correctly threw the thread is already dead so it returns
        // at once — no elapsed-time assertion either way.
        ok.join(T * 1000);
        check(ran.get() == 1,
                "a refused restart must not run the body again: " + ran.get());

        // The other half of the same JVMS rule, and the half a state check that
        // only looks for TERMINATED would miss: `start()` on a thread that is
        // still RUNNING must throw too. Held live by a latch rather than a
        // sleep so the window is deterministic.
        final CountDownLatch release = new CountDownLatch(1);
        final CountDownLatch entered = new CountDownLatch(1);
        Thread live = new Thread(() -> {
            entered.countDown();
            try {
                release.await();
            } catch (InterruptedException ie) {
                Thread.currentThread().interrupt();
            }
        }, "rjdk-exit-live");
        live.start();
        check(entered.await(T, TimeUnit.SECONDS), "the live thread must have entered its body");
        boolean threwLive = false;
        try {
            live.start();
        } catch (IllegalThreadStateException expected) {
            threwLive = true;
        }
        release.countDown();
        live.join(T * 1000);
        check(threwLive, "starting an already-running thread must throw IllegalThreadStateException");
        check(!live.isAlive(), "the live thread must have terminated after release");
        System.out.println("CK RJdkExecutors threadExit fired=" + fired.get()
                + " caught=" + caught.get());
    }

    public static void main(String[] args) throws Exception {
        fixedPool();
        factoryAndRejection();
        scheduled();
        interruption();
        completableFutures();
        threadExitCleanup();
        System.out.println("CK RJdkExecutors checks=" + checks);
        System.out.println("PASS RJdkExecutors (" + checks + " checks)");
    }
}
