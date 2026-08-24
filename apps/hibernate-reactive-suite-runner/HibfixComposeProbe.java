// Does thenCompose RELAY a not-yet-complete inner stage, or complete early?
//
// TableReactiveIdentifierGenerator.nextHiValue is a recursive CAS retry:
//
//     select().thenCompose( r -> update().thenCompose( n -> checkValue(n, id) ) )
//     checkValue: 1 -> completedFuture(id)        // won
//                 0 -> nextHiValue()              // lost, retry from scratch
//
// The outer stage must not complete until the recursion bottoms out. If
// UniCompose ever completes it while the inner stage is still running, the
// caller proceeds with a wrong value AND the retry keeps going behind it --
// which is what reaching a CLOSED connection inside checkValue would mean.
//
// This probe is that shape and nothing else: an async inner stage, a recursive
// retry, many threads, hot enough to compile.
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

public class HibfixComposeProbe {

    static final AtomicLong CHAINS = new AtomicLong();
    static final AtomicLong WRONG_VALUE = new AtomicLong();      // outer got the wrong answer
    static final AtomicLong EARLY = new AtomicLong();            // outer done before inner
    static final AtomicLong RETRIES = new AtomicLong();

    static ScheduledExecutorService timer;

    /** A stage that completes LATER, off-thread — like a real query result. */
    static CompletableFuture<Integer> async(int value) {
        CompletableFuture<Integer> f = new CompletableFuture<>();
        timer.schedule(() -> f.complete(value), 1, TimeUnit.MICROSECONDS);
        return f;
    }

    /** nextHiValue's shape: retry until the CAS "wins". */
    static CompletableFuture<Long> nextHiValue(AtomicInteger attempts, long id, AtomicInteger innerDone) {
        return async(0).thenCompose(sel ->
                async(attempts.decrementAndGet() <= 0 ? 1 : 0).thenCompose(rowCount -> {
                    if (rowCount == 1) {
                        innerDone.incrementAndGet();
                        return CompletableFuture.completedFuture(id);
                    }
                    RETRIES.incrementAndGet();
                    return nextHiValue(attempts, id, innerDone);
                }));
    }

    public static void main(String[] args) throws Exception {
        int threads = Integer.getInteger("probe.threads", 24);
        int chains = Integer.getInteger("probe.chains", 20000);
        timer = Executors.newScheduledThreadPool(4);
        ExecutorService pool = Executors.newFixedThreadPool(threads);
        CountDownLatch done = new CountDownLatch(threads);

        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int tid = t;
            pool.submit(() -> {
                try {
                    for (int n = 0; n < chains; n++) {
                        long id = ((long) tid << 32) | n;
                        // 1..4 CAS rounds, so the recursion really recurses
                        AtomicInteger attempts = new AtomicInteger(1 + (n & 3));
                        AtomicInteger innerDone = new AtomicInteger();
                        Long got = nextHiValue(attempts, id, innerDone).join();
                        // The outer stage must not finish before the recursion bottomed out.
                        if (innerDone.get() == 0) EARLY.incrementAndGet();
                        if (got == null || got != id) WRONG_VALUE.incrementAndGet();
                        CHAINS.incrementAndGet();
                    }
                } finally { done.countDown(); }
            });
        }
        done.await();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        pool.shutdownNow(); timer.shutdownNow();

        System.out.println("@@COMPOSE threads=" + threads + " chains=" + CHAINS.get()
                + " retries=" + RETRIES.get()
                + " wrong_value=" + WRONG_VALUE.get()
                + " completed_early=" + EARLY.get()
                + " ms=" + ms);
        System.out.println(WRONG_VALUE.get() == 0 && EARLY.get() == 0
                ? "@@COMPOSE CLEAN" : "@@COMPOSE DEFECT");
    }
}
