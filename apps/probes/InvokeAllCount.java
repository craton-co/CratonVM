import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.concurrent.Callable;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Does `ExecutorService.invokeAll(n callables)` actually RUN n callables?
 *
 * WHY. `org.h2.test.jdbc.TestCachedQueryResults` submits exactly 100000
 * callables through `invokeAll` and asserts a set holds 100000 entries;
 * CratonVM answers 98304 in both modes. A direct
 * `ConcurrentHashMap.newKeySet()` probe with the same thread count and the same
 * 100000 distinct adds is PERFECT on CratonVM -- 100000 added, 100000 in the
 * set, 0 contains-misses -- so the set is not the thing losing them, and the
 * `98304 == 131072 - (131072 >>> 2)` resize-threshold coincidence is just that.
 * That leaves the submission path, and `docs/known-issues/` already records an
 * `invokeAll` that copied 3 of 8 tasks on a ForkJoinTask arm.
 *
 * Four numbers, because they separate the candidates:
 *   * `executed`   -- callables that actually entered their body;
 *   * `futures`    -- the size of the list `invokeAll` returned, which the
 *                     contract says must equal the input list;
 *   * `done`       -- futures reporting `isDone()`, which the contract says is
 *                     ALL of them once `invokeAll` returns;
 *   * `set`        -- distinct side effects that survived.
 *
 * `futures` short means invokeAll dropped tasks before running them. `executed`
 * short with `futures` full means it returned handles for work it never did.
 * `set` short with `executed` full would send the blame back to the collection.
 */
public final class InvokeAllCount {
    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int tasks   = args.length > 1 ? Integer.parseInt(args[1]) : 100_000;

        AtomicInteger executed = new AtomicInteger();
        Set<Integer> set = ConcurrentHashMap.newKeySet();
        AtomicInteger seq = new AtomicInteger();

        ExecutorService pool = Executors.newFixedThreadPool(threads);
        // One shared Callable instance, exactly as the H2 test does -- the same
        // object added to the list `tasks` times, not `tasks` distinct objects.
        Callable<Object> c = () -> {
            executed.incrementAndGet();
            set.add(seq.getAndIncrement());
            return 0;
        };
        List<Callable<Object>> list = new ArrayList<>();
        for (int i = 0; i < tasks; i++) {
            list.add(c);
        }

        List<Future<Object>> futures = pool.invokeAll(list);
        int done = 0;
        for (Future<Object> f : futures) {
            if (f.isDone()) {
                done++;
            }
        }
        pool.shutdownNow();

        System.out.println("threads   |" + threads + "|");
        System.out.println("submitted |" + tasks + "|");
        System.out.println("futures   |" + futures.size() + "|");
        System.out.println("done      |" + done + "|");
        System.out.println("executed  |" + executed.get() + "|");
        System.out.println("set       |" + set.size() + "|");
        System.out.println("all-ran   |" + (executed.get() == tasks) + "|");
    }
}
