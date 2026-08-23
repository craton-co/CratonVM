import java.util.concurrent.CompletableFuture;
import java.util.concurrent.CompletionStage;

/**
 * hibfix-20260822: per-operation A/B for the `CompletableFuture` composition
 * ops that dominate `MultithreadedInsertionWithLazyConnectionTest`.
 *
 * That test's profile puts 37% of all samples in `java.util.concurrent.
 * CompletableFuture`, and `--dump-native-registry` counts 2.04M `thenCompose`
 * + 1.93M `whenComplete` + 0.39M `thenRun` in one 141s run. Dividing gives
 * ~12us per composition op, which is not the ~10x general engine gap. This
 * probe measures the per-op cost directly so the ratio is a measurement rather
 * than a division.
 *
 * Every stage here is ALREADY COMPLETE, which is the reactive-suite shape: a
 * Vert.x future that has already resolved runs its continuation inline, so the
 * cost is the composition machinery, not waiting.
 */
public final class HibfixCfProbe {
	static final long[] SINK = new long[1];

	static void bench(String name, int iters, Runnable body) {
		for (int i = 0; i < iters / 10 + 1; i++) body.run();
		long t0 = System.nanoTime();
		for (int i = 0; i < iters; i++) body.run();
		long ns = System.nanoTime() - t0;
		System.out.printf("@@ROW %-34s iters=%-8d ns_per_op=%d%n", name, iters, ns / iters);
	}

	public static void main(String[] args) {
		int scale = Integer.getInteger("probe.scale", 1);
		final CompletableFuture<Object> done = CompletableFuture.completedFuture("x");

		bench("completedFuture", 200000 * scale,
				() -> { if (CompletableFuture.completedFuture("y") != null) SINK[0]++; });

		bench("thenCompose", 200000 * scale,
				() -> done.thenCompose(v -> done).thenAccept(v -> SINK[0]++));

		bench("whenComplete", 200000 * scale,
				() -> done.whenComplete((v, e) -> SINK[0]++));

		bench("thenRun", 200000 * scale,
				() -> done.thenRun(() -> SINK[0]++));

		bench("thenAccept", 200000 * scale,
				() -> done.thenAccept(v -> SINK[0]++));

		// The shape hibernate-reactive's CompletionStages actually builds.
		bench("compose.chain.x3", 100000 * scale, () -> {
			CompletionStage<Object> s = done;
			s = s.thenCompose(v -> done);
			s = s.thenCompose(v -> done);
			s.whenComplete((v, e) -> SINK[0]++);
		});

		bench("requireNonNull", 2000000 * scale,
				() -> { if (java.util.Objects.requireNonNull(done) != null) SINK[0]++; });

		System.out.println("@@SINK " + SINK[0]);
	}
}
