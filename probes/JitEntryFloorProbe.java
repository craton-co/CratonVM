/**
 * What does one interpreter -> JIT-compiled-callee call cost?
 *
 * `ByteBufferScalarSplitProbe` cannot answer this. It measures steady state
 * INSIDE one long-lived OSR-compiled frame — five million iterations under a
 * single JIT entry — so it amortizes the per-invocation entry cost (the
 * `JitEntryGuard` push/pop, the boundary-generation bump, the argument
 * marshalling) over five million iterations and reports none of it. That is why
 * it shows the JIT winning 4.6x-98.8x while `ZipContentTests` shows the JIT
 * costing +83% CPU: those are two different regimes, and only one was measured.
 *
 * `ZipContentTests` is the other regime. Its Java stack is 35-70 JUnit /
 * reflection frames that never go hot, calling short library methods that all
 * compile. Every one of those calls crosses from the interpreter into compiled
 * code and back.
 *
 * The three arms separate the crossing from the code on either side of it:
 *
 *   both-compiled  default; driver loop OSR-compiles, callee compiles.
 *                  A JIT->JIT call — what the accessor probe measured.
 *   crossing       CRATONVM_JIT_DENY=JitEntryFloorProbe.driver keeps the CALLER
 *                  interpreted while the callee still compiles, so every
 *                  iteration pays exactly one interpreter->JIT entry and one
 *                  return. This is the ZipContentTests shape.
 *   interpreted    --nojit; both sides interpreted, no crossing at all.
 *
 * If `crossing` is SLOWER than `interpreted`, then compiling a short method
 * that an interpreted caller invokes is a net loss, and the per-invocation
 * entry — not the accessors, not the compiler, not the root scan — is what
 * makes the JIT cost +83% on a class shaped like this one.
 *
 * `work()` is deliberately tiny and pure arithmetic: any real body would dilute
 * the crossing being measured. `sink` is returned and printed so nothing here
 * is dead code the optimizer may delete — a probe whose loop is removed reports
 * a beautiful number and measures nothing.
 */
public final class JitEntryFloorProbe {

	private static final int OPS = 5_000_000;

	public static void main(String[] args) {
		// Warm both sides: the callee needs to cross its own warmup threshold,
		// and under the default arm the driver's loop needs to OSR-compile.
		driver(200_000);

		long best = Long.MAX_VALUE;
		long sink = 0;
		for (int rep = 0; rep < 3; rep++) {
			long t0 = System.nanoTime();
			sink += driver(OPS);
			long ns = System.nanoTime() - t0;
			best = Math.min(best, ns);
		}
		System.out.println("arm,ops,millis,ns_per_call");
		System.out.printf("interp->jit call ,%d,%d,%.2f%n", OPS, best / 1_000_000, (double) best / OPS);
		System.out.println("sink=" + sink);
	}

	/**
	 * The caller. Pinned to the interpreter in the `crossing` arm via
	 * `CRATONVM_JIT_DENY=JitEntryFloorProbe.driver`, which is the whole point:
	 * the deny list is the only lever that can hold one side of a call at a
	 * chosen tier while the other side compiles normally.
	 */
	private static long driver(int ops) {
		long sink = 0;
		for (int i = 0; i < ops; i++) {
			sink += work(i);
		}
		return sink;
	}

	/** The callee: short enough that the crossing dominates its own body. */
	private static int work(int i) {
		return (i * 31) ^ (i >>> 3);
	}

}
