import java.util.concurrent.CompletableFuture;

/** Separates "the native + by-name delegation" from "CratonVM running the real
 *  CompletableFuture bytecode". `copy()` and `minimalCompletionStage()` build
 *  the same uni*Stage dependent machinery as `thenApply`/`thenCompose` but are
 *  not in CratonVM's native registry, so they price the bytecode alone. */
public final class HibfixCfBound {
	static final long[] SINK = new long[1];
	static void bench(String name, int iters, Runnable body) {
		for (int i = 0; i < iters / 10 + 1; i++) body.run();
		long t0 = System.nanoTime();
		for (int i = 0; i < iters; i++) body.run();
		System.out.printf("@@ROW %-30s ns_per_op=%d%n", name, (System.nanoTime() - t0) / iters);
	}
	public static void main(String[] a) {
		final CompletableFuture<Object> done = CompletableFuture.completedFuture("x");
		bench("copy [no native]", 200000, () -> { if (done.copy() != null) SINK[0]++; });
		bench("minimalCompletionStage", 200000, () -> { if (done.minimalCompletionStage() != null) SINK[0]++; });
		bench("thenApply [native]", 200000, () -> done.thenApply(v -> v));
		bench("thenCompose [native]", 200000, () -> done.thenCompose(v -> done));
		System.out.println("@@SINK " + SINK[0]);
	}
}
