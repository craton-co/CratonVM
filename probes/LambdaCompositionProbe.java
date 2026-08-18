import java.util.concurrent.CompletableFuture;
import java.util.function.Function;

/**
 * The lambda shape the hibernate-reactive classes actually spend their time in.
 *
 * `residual-seven-after-the-afc-fix-20260817.md` put ~55% of
 * `MultithreadedInsertionTest`'s samples in `CompletableFuture` composition,
 * and composition is `Function<Object,Object>` — a GENERIC functional
 * interface, so every stage arrives through the erased SAM and a `checkcast`,
 * and every `int` in the chain arrives boxed. That is `boxedLambda` in
 * SamDispatchDecompositionProbe, the row that costs 4x every other one.
 *
 * A probe on the non-generic `IntUnaryOperator` shape says nothing about it,
 * which is the trap the 2026-08-09 lambda fast-path widening fell into: 4x on
 * a microbenchmark, 0% on the Spring workload. So this one is built out of the
 * real thing — `thenApply` / `thenCompose` chains over already-completed
 * futures, which is what an in-memory reactive pipeline degenerates to once
 * the I/O is served from a pool.
 *
 * Usage: LambdaCompositionProbe [chains] [depth]
 */
public class LambdaCompositionProbe {

    public static void main(String[] args) {
        int chains = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        int depth = args.length > 1 ? Integer.parseInt(args[1]) : 8;

        // Warm up on the same shapes, so the measured window is steady state.
        long warm = runChains(2_000, depth) + runCompose(2_000, depth);

        long t0 = System.nanoTime();
        long applySink = runChains(chains, depth);
        long applyNs = System.nanoTime() - t0;

        long t1 = System.nanoTime();
        long composeSink = runCompose(chains, depth);
        long composeNs = System.nanoTime() - t1;

        long stages = (long) chains * depth;
        System.out.printf("thenApply    %.1f ns/stage  stages=%d sink=%d%n",
                (double) applyNs / stages, stages, applySink);
        System.out.printf("thenCompose  %.1f ns/stage  stages=%d sink=%d%n",
                (double) composeNs / stages, stages, composeSink);
        System.out.println("PROBE-DONE warm=" + warm);
    }

    /** `Function<Integer,Integer>` stages — the erased-SAM, boxed-argument shape. */
    private static long runChains(int chains, int depth) {
        long sink = 0;
        for (int c = 0; c < chains; c++) {
            CompletableFuture<Integer> f = CompletableFuture.completedFuture(c);
            for (int d = 0; d < depth; d++) {
                f = f.thenApply(v -> v + 1);
            }
            sink += f.join();
        }
        return sink;
    }

    /** The same, through `thenCompose` — one more lambda layer per stage. */
    private static long runCompose(int chains, int depth) {
        long sink = 0;
        for (int c = 0; c < chains; c++) {
            CompletableFuture<Integer> f = CompletableFuture.completedFuture(c);
            for (int d = 0; d < depth; d++) {
                Function<Integer, CompletableFuture<Integer>> step =
                        v -> CompletableFuture.completedFuture(v + 1);
                f = f.thenCompose(step);
            }
            sink += f.join();
        }
        return sink;
    }
}
