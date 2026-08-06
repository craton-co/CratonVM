import java.util.concurrent.*;
import java.util.*;

/**
 * Three of the four verification items the ThreadPoolExecutor record asks for:
 *  - the self-recursion guard (native code calling .execute() on a real pool),
 *  - the async-degradation guard (task must not run on the submitting thread),
 *  - the cache-poisoning guard (one call site, alternating receivers).
 */
public class AsyncProbe {
    // ONE call site for execute(): every receiver below goes through it, so a
    // monomorphic inline cache poisoned by the first receiver shows up here.
    static String runOn(Executor e) throws Exception {
        final ArrayBlockingQueue<String> q = new ArrayBlockingQueue<>(1);
        e.execute(() -> {
            try { q.put(Thread.currentThread().getName()); } catch (InterruptedException ex) {}
        });
        String t = q.poll(15, TimeUnit.SECONDS);
        return t == null ? "TIMEOUT" : t;
    }

    public static void main(String[] args) throws Exception {
        String main = Thread.currentThread().getName();
        List<ExecutorService> pools = new ArrayList<>();
        pools.add(Executors.newFixedThreadPool(2));
        pools.add(new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS,
                new LinkedBlockingQueue<Runnable>()));
        pools.add(Executors.newCachedThreadPool());
        pools.add(Executors.newSingleThreadExecutor());

        // Alternate through the SAME call site several times.
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < pools.size(); i++) {
                String t = runOn(pools.get(i));
                System.out.println("round=" + round + " pool=" + i
                        + " ran_on=" + t + " async=" + (!t.equals(main) && !t.equals("TIMEOUT")));
            }
        }

        // CompletableFuture / ForkJoinPool: the route through
        // spawn_runnable_on_real_thread, i.e. native code calling .execute()
        // on a real ThreadPoolExecutor. This is the one that used to be a
        // native stack overflow and a process abort.
        CompletableFuture<String> cf = CompletableFuture.supplyAsync(() -> "cf:" + Thread.currentThread().getName());
        System.out.println("supplyAsync=" + cf.get(20, TimeUnit.SECONDS));
        CompletableFuture<String> cf2 = CompletableFuture.supplyAsync(() -> "a")
                .thenApplyAsync(s -> s + "b")
                .thenApplyAsync(s -> s + "c");
        System.out.println("chain=" + cf2.get(20, TimeUnit.SECONDS));
        ForkJoinPool fjp = ForkJoinPool.commonPool();
        System.out.println("fjp=" + fjp.submit(() -> "fj-ok").get(20, TimeUnit.SECONDS));

        for (ExecutorService p : pools) p.shutdown();
        for (ExecutorService p : pools) System.out.println("shutdown=" + p.isShutdown());
        System.out.println("ASYNC-DONE");
    }
}
