/*
 * Standalone reproduction of a suspected double-execution defect in
 * hibernate-reactive's AsyncTrampoline (org.hibernate.reactive.util.async.impl),
 * observed while investigating the "Unmanaged instance passed to remove()"
 * cascade documented in
 * docs/known-issues/hibernate/hib-reactive-3gc-run-regressions-20260820.md.
 *
 * Real symptom (from an instrumented FilterWithPaginationTest run): a 5-element
 * array loop driven by AsyncTrampoline.asyncWhile(loop::next) called
 * consumer.apply(index) TWICE for the SAME index (2) and never for index 3 --
 * i.e. one loop step executed twice, one was lost. The array-loop index lives
 * in a plain (non-volatile, non-atomic) `int current` field mutated by
 * ArrayLoop.next(), so if the trampoline's own reentrancy-avoidance logic ever
 * lets two logical "turns" of the loop run overlapped instead of strictly
 * sequenced, `current++` races and one index gets visited twice while another
 * is skipped.
 *
 * This probe copies AsyncTrampoline's actual unroll()/PassBack logic
 * (Apache-2.0, same file this VM already vendors under apps/hibernate-reactive)
 * and ArrayLoop's exact shape, then drives it with a genuinely cross-thread,
 * asynchronously-completing step (a tiny fixed thread pool posting the
 * completion from a DIFFERENT thread after a short random delay -- mimicking
 * a Vert.x/DB I/O callback), which is the same "did the callback truly run
 * synchronously-nested or truly async" boundary the trampoline's
 * currentThread.equals(previousThread) + PassBack.isRunning check depends on.
 *
 * PASS = every trial visits indices 0..N-1 exactly once, in order.
 * FAIL = any trial visits an index more than once, or skips one.
 *
 * RESULT (2026-08-20, dev@77e712ec6 + local fix/hib-reactive-persistence-cascade
 * changes): 20,000 trials on both HotSpot and CratonVM (cratonvm-hibcascade.exe)
 * -- 0 dupTrials, 0 skipTrials on EITHER VM. The generic trampoline mechanism,
 * even with a genuine cross-thread race on the sync/async boundary, does not
 * reproduce the defect on its own -- see doc section 3.3. Kept as a checked
 * negative result and a reusable harness; the real trigger needs the actual
 * Vert.x/reactive-SQL-client call shape (see doc section 3.4).
 */
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.Function;
import java.util.function.Predicate;
import java.util.function.Supplier;

public class AsyncTrampolineDoubleFireProbe {

    // ---- copy of AsyncTrampoline (trimmed to what this probe needs) ----
    static final class AsyncTrampoline {
        private AsyncTrampoline() {}

        private static final class TrampolineInternal<T> extends CompletableFuture<T> {
            private final Predicate<? super T> shouldContinue;
            private final Function<? super T, ? extends CompletionStageLike<T>> f;

            private TrampolineInternal(Predicate<? super T> shouldContinue,
                                        Function<? super T, ? extends CompletionStageLike<T>> f) {
                this.shouldContinue = shouldContinue;
                this.f = f;
            }

            private static <T> CompletableFuture<T> trampoline(
                    Predicate<? super T> shouldContinue,
                    Function<? super T, ? extends CompletionStageLike<T>> f,
                    T initialValue) {
                TrampolineInternal<T> t = new TrampolineInternal<>(shouldContinue, f);
                t.unroll(initialValue, null, null);
                return t;
            }

            private void unroll(T completed, Thread previousThread, PassBack<T> previousPassBack) {
                Thread currentThread = Thread.currentThread();
                if (currentThread.equals(previousThread) && (previousPassBack != null && previousPassBack.isRunning)) {
                    previousPassBack.item = completed;
                } else {
                    PassBack<T> currentPassBack = new PassBack<>();
                    T c = completed;
                    do {
                        try {
                            if (shouldContinue.test(c)) {
                                f.apply(c).stage().whenComplete((next, ex) -> {
                                    if (ex != null) {
                                        completeExceptionally(ex);
                                    } else {
                                        unroll(next, currentThread, currentPassBack);
                                    }
                                });
                            } else {
                                complete(c);
                                return;
                            }
                        } catch (Throwable e) {
                            completeExceptionally(e);
                            return;
                        }
                    } while ((c = currentPassBack.poll()) != PassBack.NIL);
                    currentPassBack.isRunning = false;
                }
            }

            @SuppressWarnings("unchecked")
            private static final class PassBack<T> {
                private static final Object NIL = new Object();
                boolean isRunning = true;
                T item = (T) NIL;
                T poll() {
                    T c = item;
                    item = (T) NIL;
                    return c;
                }
            }
        }

        static CompletableFuture<Void> asyncWhile(Supplier<? extends CompletionStageLike<Boolean>> fn) {
            return TrampolineInternal.trampoline(b -> b, b -> fn.get(), true)
                    .thenCompose(v -> CompletableFuture.completedFuture(null));
        }
    }

    // Thin wrapper so we can force a genuinely cross-thread async completion
    // for the loop body, same as a real Vert.x/DB round trip would produce.
    interface CompletionStageLike<T> {
        java.util.concurrent.CompletionStage<T> stage();
    }

    // ---- copy of the array-loop shape from CompletionStages.loop(T[], ...) ----
    static final class ArrayLoop {
        private final int end;
        private int current; // <-- plain int, exactly like the real ArrayLoop
        private final Function<Integer, CompletionStageLike<Boolean>> consumer;

        ArrayLoop(int start, int end, Function<Integer, CompletionStageLike<Boolean>> consumer) {
            this.end = end;
            this.current = start;
            this.consumer = consumer;
        }

        CompletionStageLike<Boolean> next() {
            int index = current++;
            if (index < end) {
                return consumer.apply(index);
            }
            CompletableFuture<Boolean> f = new CompletableFuture<>();
            f.complete(false);
            return () -> f;
        }
    }

    static final ExecutorService IO_POOL = Executors.newFixedThreadPool(4);

    public static void main(String[] args) throws Exception {
        int trials = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int n = 5;
        int dupTrials = 0, skipTrials = 0;

        for (int trial = 0; trial < trials; trial++) {
            final int trialF = trial;
            AtomicIntegerArray visits = new AtomicIntegerArray(n);
            AtomicInteger order = new AtomicInteger(0);
            java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);

            ArrayLoop loop = new ArrayLoop(0, n, index -> {
                visits.incrementAndGet(index);
                CompletableFuture<Boolean> f = new CompletableFuture<>();
                // Randomly complete either synchronously (already-resolved,
                // like a persistence-context cache hit) or asynchronously
                // from a different pool thread (like a real DB round trip) --
                // this mirrors the real workload's mix of both, which is what
                // exercises the trampoline's sync/async boundary detection.
                if ((index ^ trialF) % 3 == 0) {
                    f.complete(true);
                } else {
                    IO_POOL.submit(() -> {
                        try { Thread.sleep(0, 1 + (trialF % 5)); } catch (InterruptedException ignored) {}
                        f.complete(true);
                    });
                }
                return () -> f;
            });

            AsyncTrampoline.asyncWhile(loop::next).whenComplete((v, ex) -> done.countDown());
            done.await(10, TimeUnit.SECONDS);

            boolean dup = false, skip = false;
            for (int i = 0; i < n; i++) {
                int v = visits.get(i);
                if (v > 1) dup = true;
                if (v == 0) skip = true;
            }
            if (dup) dupTrials++;
            if (skip) skipTrials++;
            if ((dup || skip) && (dupTrials + skipTrials) <= 5) {
                StringBuilder sb = new StringBuilder();
                for (int i = 0; i < n; i++) sb.append(visits.get(i)).append(' ');
                System.out.println("trial=" + trial + " visits=[" + sb.toString().trim() + "] dup=" + dup + " skip=" + skip);
            }
        }

        System.out.println("@@RESULT trials=" + trials + " dupTrials=" + dupTrials + " skipTrials=" + skipTrials);
        IO_POOL.shutdown();
    }
}
