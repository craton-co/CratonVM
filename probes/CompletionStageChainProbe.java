import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

/**
 * Per-stage cost of the `CompletableFuture` composition hibernate-reactive's
 * reactive pipeline is built out of.
 *
 * Written 2026-08-17. Five of the seven residual hibernate-reactive classes are
 * not hangs: they produce correct results and simply exceed the suite's 120 s
 * wall cap (`MultithreadedInsertionTest` PASSES in 219 s against HotSpot's
 * 18.5 s). Sampling the running VM put ~55% of the leaves in
 * `thenCompose`/`uniComposeStage`/`whenComplete`/`uniWhenComplete` plus
 * hibernate-reactive's own `AsyncTrampoline.unroll`, with no single hot body —
 * i.e. the cost is per COMPOSITION STEP, and that is what this measures.
 *
 * Shapes, all on an already-completed future so no thread hand-off or parking
 * is timed — the workload's stages complete inline the same way:
 *
 *   compose   — `thenCompose`, the trampoline's own step
 *   when      — `whenComplete`, the trampoline's unroll callback
 *   apply     — `thenApply`, the cheapest composition for a baseline
 *   trampoline— compose+when nested, the actual `AsyncTrampoline` shape
 *
 * The loop body is in a CALLED METHOD, never inline in `main`: a loop inline in
 * `main` is OSR-only on this VM and has measured 180x slower, which would
 * swamp the thing being measured.
 */
public class CompletionStageChainProbe {

    private static final int WARMUP = 20_000;
    private static final int ITERS = 200_000;

    private static Object sink;

    private static long compose(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            CompletionStage<Integer> s = CompletableFuture.completedFuture(i)
                    .thenCompose(v -> CompletableFuture.completedFuture(v + 1));
            sink = s;
        }
        return System.nanoTime() - t0;
    }

    private static long when(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            CompletionStage<Integer> s = CompletableFuture.completedFuture(i)
                    .whenComplete((v, t) -> { sink = v; });
            sink = s;
        }
        return System.nanoTime() - t0;
    }

    private static long apply(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            CompletionStage<Integer> s = CompletableFuture.completedFuture(i)
                    .thenApply(v -> v + 1);
            sink = s;
        }
        return System.nanoTime() - t0;
    }

    /**
     * Allocation-only baseline: the same `completedFuture` allocation and
     * boxing per iteration, with NO composition at all.
     *
     * This is the discriminator. If `alloc` costs the same as `apply`, the
     * measured number is allocation/GC and says nothing about composition; if
     * `alloc` is cheap and `apply` is not, the cost really is per composition
     * step.
     */
    private static long alloc(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink = CompletableFuture.completedFuture(i);
        }
        return System.nanoTime() - t0;
    }

    /** Boxing only — the floor under `alloc`. */
    private static long box(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            sink = Integer.valueOf(i);
        }
        return System.nanoTime() - t0;
    }

    /** compose + when nested — the `AsyncTrampoline.unroll` shape. */
    private static long trampoline(int n) {
        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            CompletionStage<Integer> s = CompletableFuture.completedFuture(i)
                    .thenCompose(v -> CompletableFuture.completedFuture(v + 1))
                    .whenComplete((v, t) -> { sink = v; });
            sink = s;
        }
        return System.nanoTime() - t0;
    }

    private static void report(String name, long nanos, int n) {
        System.out.printf("%-11s %8.1f ns/op  (%d ops, %.1f ms)%n",
                name, (double) nanos / n, n, nanos / 1e6);
    }

    public static void main(String[] args) {
        box(WARMUP);
        alloc(WARMUP);
        compose(WARMUP);
        when(WARMUP);
        apply(WARMUP);
        trampoline(WARMUP);

        report("box", box(ITERS), ITERS);
        report("alloc", alloc(ITERS), ITERS);
        report("apply", apply(ITERS), ITERS);
        report("compose", compose(ITERS), ITERS);
        report("when", when(ITERS), ITERS);
        report("trampoline", trampoline(ITERS), ITERS);
        System.out.println("PROBE-DONE sink=" + (sink != null));
    }
}
