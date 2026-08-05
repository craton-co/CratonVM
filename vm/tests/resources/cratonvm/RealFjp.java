package cratonvm;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.ForkJoinPool;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Smoke test for the CRATONVM_REAL_FORKJOINPOOL real-path gate.
 *
 * Run under CRATONVM_REAL_FORKJOINPOOL=1 to verify that ForkJoinPool runs
 * real JDK bytecode instead of the synthetic overlay. The test submits a
 * trivial callable to the common pool and waits for the result.
 *
 * <p>Also covers the BULK-submission surface. `commonPool()` is a VM Bridge
 * shortcut whose object never ran the real pool constructor, so any pool
 * method missing from the bridge allow-list falls through to real JDK
 * bytecode and throws `RejectedExecutionException` at `submissionQueue()`.
 * `invokeAll(Collection)` — the overload Weld's `ConcurrentBeanDeployer`
 * calls — was missing, which failed the whole `org.hibernate.orm.test.cdi.*`
 * cluster; `invokeAny` was missing too and failed SILENTLY, returning null.
 *
 * <p>The bulk assertions check that the callables ACTUALLY RAN and that a
 * failing one surfaces `ExecutionException` — not merely that no exception
 * escaped. A bridge that quietly dropped the work, or that swallowed a
 * throwing callable into a null result, would otherwise score a clean pass.
 */
public class RealFjp {

    static final AtomicInteger RAN = new AtomicInteger();

    public static void main(String[] args) throws Exception {
        ForkJoinPool pool = ForkJoinPool.commonPool();
        Future<Integer> f = pool.submit(() -> 42);
        int result = f.get(10, TimeUnit.SECONDS);
        System.out.println("r:result=" + result);
        System.out.println("r:parallelism=" + (pool.getParallelism() > 0));
        // Second task to verify queueing works.
        int sum = ForkJoinPool.commonPool().submit(() -> 100 + 23).get(10, TimeUnit.SECONDS);
        System.out.println("r:sum=" + sum);

        // --- invokeAll: the CDI-cluster regression ----------------------
        int before = RAN.get();
        List<Callable<Integer>> batch = new ArrayList<>();
        for (int i = 1; i <= 4; i++) {
            batch.add(counted(i));
        }
        List<Future<Integer>> futures = pool.invokeAll(batch);
        int total = 0;
        boolean allDone = true;
        for (Future<Integer> fut : futures) {
            total += fut.get(10, TimeUnit.SECONDS);
            allDone &= fut.isDone();
        }
        System.out.println("r:invokeAllSize=" + futures.size());
        System.out.println("r:invokeAllSum=" + total);
        System.out.println("r:invokeAllRan=" + (RAN.get() - before));
        System.out.println("r:invokeAllDone=" + allDone);

        // Weld reaches invokeAll through an ExecutorService-typed receiver,
        // i.e. invokeinterface rather than invokevirtual.
        ExecutorService es = ForkJoinPool.commonPool();
        System.out.println("r:ifaceInvokeAllSize=" + es.invokeAll(batch).size());

        // --- a throwing callable must reach the caller as an
        // --- ExecutionException: Weld's checkForExceptions depends on it --
        List<Future<Integer>> mixed = pool.invokeAll(
            Arrays.<Callable<Integer>>asList(counted(7), () -> {
                throw new IllegalStateException("fjp-boom");
            }));
        String failure = "none";
        try {
            mixed.get(1).get(10, TimeUnit.SECONDS);
        } catch (ExecutionException e) {
            failure = e.getCause() == null ? "nullcause" : e.getCause().getClass().getSimpleName();
        }
        System.out.println("r:invokeAllFailure=" + failure);
        System.out.println("r:invokeAllOkSibling=" + mixed.get(0).get(10, TimeUnit.SECONDS));

        // --- invokeAny returned null before the fix ----------------------
        Integer any = pool.invokeAny(Arrays.<Callable<Integer>>asList(counted(11), counted(12)));
        System.out.println("r:invokeAnyInRange=" + (any != null && (any == 11 || any == 12)));

        System.out.println("REAL_FJP_OK 3");
    }

    static Callable<Integer> counted(final int v) {
        return () -> {
            RAN.incrementAndGet();
            return v;
        };
    }
}
