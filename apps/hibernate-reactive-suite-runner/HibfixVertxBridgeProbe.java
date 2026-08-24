// Does io.vertx.core.Future.toCompletionStage() complete on time, with the
// right value?
//
// Every hibernate-reactive DB call crosses this bridge -- selectIdentifier,
// update, begin and commit all end in `.toCompletionStage()`. It is the one
// link on the id-generator retry path not yet covered by a probe:
// HibfixComposeProbe cleared plain CompletableFuture composition and
// HibfixThreadIdentityProbe cleared the event-loop thread check.
//
// If the bridge's CompletableFuture can complete before the Vert.x Future it
// mirrors, the caller proceeds while the real work is still running -- which is
// exactly what a CAS retry reaching a CLOSED connection means.
import io.vertx.core.Future;
import io.vertx.core.Promise;
import java.util.concurrent.*;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class HibfixVertxBridgeProbe {

    static final AtomicLong CROSSINGS = new AtomicLong();
    static final AtomicLong EARLY = new AtomicLong();
    static final AtomicLong WRONG_VALUE = new AtomicLong();
    static final AtomicLong LOST = new AtomicLong();

    static ScheduledExecutorService timer;

    static CompletionStage<Integer> cross(int value, AtomicBoolean promiseDone) {
        Promise<Integer> p = Promise.promise();
        Future<Integer> f = p.future();
        CompletionStage<Integer> cs = f.toCompletionStage();
        timer.schedule(() -> { promiseDone.set(true); p.complete(value); }, 1, TimeUnit.MICROSECONDS);
        return cs;
    }

    /** The id generator's shape: retry through the bridge until the CAS wins. */
    static CompletionStage<Integer> retryChain(int rounds, int value, AtomicBoolean done) {
        AtomicBoolean inner = new AtomicBoolean();
        return cross(rounds, inner).thenCompose(r -> {
            if (r <= 0) { done.set(true); return CompletableFuture.completedFuture(value); }
            return retryChain(r - 1, value, done);
        });
    }

    public static void main(String[] args) throws Exception {
        int threads = Integer.getInteger("probe.threads", 24);
        int chains = Integer.getInteger("probe.chains", 3000);
        timer = Executors.newScheduledThreadPool(4);
        ExecutorService pool = Executors.newFixedThreadPool(threads);
        CountDownLatch done = new CountDownLatch(threads);

        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int tid = t;
            pool.submit(() -> {
                try {
                    for (int n = 0; n < chains; n++) {
                        int value = (tid << 16) | (n & 0xffff);
                        AtomicBoolean promiseDone = new AtomicBoolean();
                        Integer got = cross(value, promiseDone)
                                .toCompletableFuture().get(30, TimeUnit.SECONDS);
                        if (!promiseDone.get()) EARLY.incrementAndGet();
                        if (got == null || got != value) WRONG_VALUE.incrementAndGet();

                        AtomicBoolean chainDone = new AtomicBoolean();
                        Integer got2 = retryChain(1 + (n & 3), value, chainDone)
                                .toCompletableFuture().get(30, TimeUnit.SECONDS);
                        if (!chainDone.get()) EARLY.incrementAndGet();
                        if (got2 == null || got2 != value) WRONG_VALUE.incrementAndGet();
                        CROSSINGS.addAndGet(2);
                    }
                } catch (Exception e) {
                    LOST.incrementAndGet();
                    System.out.println("@@BRIDGE EXCEPTION " + e);
                } finally { done.countDown(); }
            });
        }
        done.await();
        long ms = (System.nanoTime() - t0) / 1_000_000;
        pool.shutdownNow(); timer.shutdownNow();

        System.out.println("@@BRIDGE threads=" + threads + " crossings=" + CROSSINGS.get()
                + " completed_early=" + EARLY.get()
                + " wrong_value=" + WRONG_VALUE.get()
                + " lost=" + LOST.get() + " ms=" + ms);
        System.out.println(EARLY.get() == 0 && WRONG_VALUE.get() == 0 && LOST.get() == 0
                ? "@@BRIDGE CLEAN" : "@@BRIDGE DEFECT");
    }
}
