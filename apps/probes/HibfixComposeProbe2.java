// Composition cost with NO scheduler in the measurement.
//
// HibfixComposeProbe used a ScheduledThreadPoolExecutor to complete the inner
// stage off-thread, and its profile turned out to be ~45% j.u.c. scheduler
// (AQS, ReentrantLock, DelayedWorkQueue) against ~24% CompletableFuture. So
// its 95x was a real ratio for a mixed workload, but not a measurement of
// composition.
//
// This variant keeps the property that matters -- the inner stage is NOT yet
// complete when it is composed, so the UniCompose relay path is exercised
// rather than the completed-future fast path -- and removes every lock and
// every cross-thread handoff. Each thread owns its futures end to end.
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicLong;

public class HibfixComposeProbe2 {

    static final AtomicLong CHAINS = new AtomicLong();
    static final AtomicLong WRONG = new AtomicLong();

    /** nextHiValue's shape: a retry recursion where each stage completes late. */
    static CompletableFuture<Long> chain(int rounds, long id, CompletableFuture<Integer>[] gates, int[] n) {
        CompletableFuture<Integer> gate = new CompletableFuture<>();
        gates[n[0]++] = gate;
        return gate.thenCompose(r -> {
            if (r <= 0) return CompletableFuture.completedFuture(id);
            return chain(r - 1, id, gates, n);
        });
    }

    public static void main(String[] args) throws Exception {
        int threads = Integer.getInteger("probe.threads", 24);
        int chains = Integer.getInteger("probe.chains", 200000);
        Thread[] ts = new Thread[threads];
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int tid = t;
            ts[t] = new Thread(() -> {
                @SuppressWarnings("unchecked")
                CompletableFuture<Integer>[] gates = new CompletableFuture[8];
                int[] n = new int[1];
                for (int i = 0; i < chains; i++) {
                    long id = ((long) tid << 32) | i;
                    int rounds = 1 + (i & 3);
                    n[0] = 0;
                    CompletableFuture<Long> outer = chain(rounds, id, gates, n);
                    // Complete the gates in order, AFTER composition: every
                    // relay goes through the not-yet-complete path.
                    for (int g = 0; g < n[0]; g++) gates[g].complete(rounds - g - 1);
                    Long got = outer.getNow(null);
                    if (got == null || got != id) WRONG.incrementAndGet();
                    CHAINS.incrementAndGet();
                }
            }, "compose-" + t);
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("@@COMPOSE2 threads=" + threads + " chains=" + CHAINS.get()
                + " wrong=" + WRONG.get() + " ms=" + ms);
        System.out.println(WRONG.get() == 0 ? "@@COMPOSE2 CLEAN" : "@@COMPOSE2 DEFECT");
    }
}
