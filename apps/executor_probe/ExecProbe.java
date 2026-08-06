import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Wave 1 Task C fixture — the executor/ThreadFactory/UncaughtExceptionHandler
 * probe that `vm/tests/wave1_c_executor.rs` asserts against.
 *
 * <p><b>This file was missing.</b> The test has shipped since wave 1 with five
 * assertions over four sub-tests, and `apps/executor_probe/` has never existed
 * in the repository — so `ensure_probe_compiled()` returned `false` on every
 * run, `run_probe()` printed "class file unavailable; skipping", and all five
 * tests returned green without executing anything. Restored 2026-08-06 while
 * closing jdk-only wave 2's L10, whose subject is exactly this surface.
 *
 * <p>The four sub-tests and their expected lines are fixed by the Rust test and
 * must not be renamed:
 *
 * <ul>
 *   <li>{@code test1=42} — {@code newFixedThreadPool(4).submit(callable).get()}
 *       round-trips a value.
 *   <li>{@code test2.latch=true counter=4000} — 4 threads x 4000 tasks against
 *       an {@code AtomicInteger} and a {@code CountDownLatch}. This is the one
 *       that fails if {@code execute()} degrades to running inline: the latch
 *       still reaches zero, but only because the submitting thread ran every
 *       task, so the counter is the assertion that matters and the latch is the
 *       liveness bound around it.
 *   <li>{@code test3.threadName=ExecProbe-worker} — a custom
 *       {@code ThreadFactory} is honoured, i.e. the pool asks it for its worker
 *       rather than spawning one itself.
 *   <li>{@code test4.caught=true} — an uncaught exception from a
 *       {@code Runnable} reaches the thread's own
 *       {@code UncaughtExceptionHandler}.
 * </ul>
 *
 * <p>Every wait is bounded and every wait's RESULT is printed rather than its
 * duration: an unbounded {@code awaitTermination} turns a defect into a
 * 30-second timeout and a truncated transcript, which is how a hang gets filed
 * as a diff. Nothing here prints a thread id, an address or an elapsed time.
 *
 * <p>Compiled with {@code --release 21} by the Rust test, so no newer language
 * features.
 */
public final class ExecProbe {

    public static void main(String[] args) throws Exception {
        test1();
        test2();
        test3();
        test4();
        System.out.println("OK");
    }

    /** `submit(Callable).get()` round-trips a value through a real pool. */
    static void test1() throws Exception {
        ExecutorService es = Executors.newFixedThreadPool(4);
        try {
            Future<Integer> f = es.submit(() -> 42);
            System.out.println("test1=" + f.get(10, TimeUnit.SECONDS));
        } finally {
            es.shutdown();
        }
    }

    /**
     * 4 threads, 4000 tasks. The counter is the real assertion — a pool that
     * runs everything inline on the caller also reaches 4000, but the latch
     * bound is what stops a lost task from hanging the probe instead of failing
     * it.
     */
    static void test2() throws Exception {
        final int tasks = 4000;
        ExecutorService es = Executors.newFixedThreadPool(4);
        try {
            final AtomicInteger counter = new AtomicInteger();
            final CountDownLatch latch = new CountDownLatch(tasks);
            for (int i = 0; i < tasks; i++) {
                es.execute(() -> {
                    counter.incrementAndGet();
                    latch.countDown();
                });
            }
            boolean ok = latch.await(10, TimeUnit.SECONDS);
            System.out.println("test2.latch=" + ok + " counter=" + counter.get());
        } finally {
            es.shutdown();
        }
    }

    /**
     * The pool must obtain its worker from the supplied factory. Asserted by the
     * name the factory gives the thread, read from INSIDE a task — reading it
     * from the factory would only prove the factory ran, not that the pool used
     * what it returned.
     */
    static void test3() throws Exception {
        ThreadFactory factory = r -> {
            Thread t = new Thread(r, "ExecProbe-worker");
            t.setDaemon(true);
            return t;
        };
        ExecutorService es = Executors.newSingleThreadExecutor(factory);
        try {
            Future<String> f = es.submit(() -> Thread.currentThread().getName());
            System.out.println("test3.threadName=" + f.get(10, TimeUnit.SECONDS));
        } finally {
            es.shutdown();
        }
    }

    /**
     * W1-C: an uncaught exception from a `Runnable` must reach the thread's own
     * handler. `msg=` is printed for triage but deliberately NOT pinned by the
     * Rust test — `getMessage()` on a synthetic-allocated `RuntimeException` has
     * its own tracked defect, and pinning it here would make this test fail for
     * a reason that has nothing to do with handler dispatch.
     */
    static void test4() throws Exception {
        final AtomicReference<Throwable> caught = new AtomicReference<>();
        final CountDownLatch handled = new CountDownLatch(1);
        Thread t = new Thread(() -> {
            throw new RuntimeException("ExecProbe-boom");
        }, "ExecProbe-thrower");
        t.setUncaughtExceptionHandler((thread, thrown) -> {
            caught.set(thrown);
            handled.countDown();
        });
        t.start();
        t.join(10_000);
        boolean fired = handled.await(10, TimeUnit.SECONDS);
        Throwable c = caught.get();
        System.out.println("test4.caught=" + (fired && c != null)
                + " msg=" + (c == null ? "<none>" : c.getMessage()));
    }
}
