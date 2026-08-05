/**
 * Prices the compiled-caller → interpreted-callee edge.
 *
 * A dispatch-heavy workload (Hibernate's criteria/metamodel code is the
 * canonical one) is a hot loop over MANY moderately-called callees: the loop
 * crosses the JIT threshold, the callees individually never do. So compiled
 * code spends its life calling methods that have no compiled entry, and the
 * cost of THAT edge — not of compiled arithmetic — decides whether turning the
 * JIT on is a win or a loss.
 *
 * The shape is reproduced here without needing hundreds of classes: run with
 * {@code CRATONVM_JIT=deny=JitCallEdgeProbe$Callee} and the callee is pinned to
 * the interpreter while the caller still compiles. Three configurations to
 * compare:
 *
 * <pre>
 *   --nojit                                   both interpreted   (control)
 *   (default)                                 both compiled
 *   CRATONVM_JIT=deny=JitCallEdgeProbe$Callee compiled → interpreted
 * </pre>
 *
 * Multi-pass by construction: a single warm-up-then-measure pass reports a
 * number that has not converged, and one such number has already produced a
 * wrong root cause in this repo's history.
 */
public final class JitCallEdgeProbe {
	private JitCallEdgeProbe() {}

	public static class Callee {
		int state;

		public int f(int i) {
			state += i;
			return state ^ (i * 3);
		}
	}

	/** Distinct receiver class so the call site is not trivially monomorphic-final. */
	public static final class Callee2 extends Callee {
		@Override
		public int f(int i) {
			state += i + 1;
			return state ^ (i * 5);
		}
	}

	static long loop(Callee c, int n) {
		long sum = 0;
		for (int i = 0; i < n; i++) {
			sum += c.f(i);
		}
		return sum;
	}

	static long sink;

	public static void main(String[] args) {
		int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
		int passes = args.length > 1 ? Integer.parseInt(args[1]) : 4;
		Callee mono = new Callee();
		Callee bi = new Callee2();
		for (int pass = 0; pass < passes; pass++) {
			long t0 = System.nanoTime();
			sink += loop(mono, iters);
			long tMono = System.nanoTime() - t0;

			t0 = System.nanoTime();
			sink += loop(bi, iters);
			long tBi = System.nanoTime() - t0;

			System.out.printf("@@EDGE pass=%d iters=%d mono_ns_per_call=%d bi_ns_per_call=%d%n",
					pass, iters, tMono / iters, tBi / iters);
			System.out.flush();
		}
		System.out.println("@@PROBEEND ok sink=" + sink);
	}
}
