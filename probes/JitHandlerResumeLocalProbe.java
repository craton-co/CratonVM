import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Regression probe for the JIT-to-JIT handler resume losing a pre-`try` local.
 *
 * Shape, taken from Spring Boot's `BindConverter.convert`:
 *
 *   for (Svc d : this.delegates) {      // iterator lives in a local assigned
 *       try { d.run(x); }               // BEFORE the protected range
 *       catch (Boom ex) { ... }         // handler reads another pre-try local
 *   }
 *
 * `precise_handler_frames_enabled()` admits such a method (its handler reads a
 * non-parameter local) on the promise that every throwing site in a protected
 * range publishes a precise exceptional frame. The JIT-to-JIT dispatch sink
 * `run_jit_callee_handler` did not consume that frame and seeded the handler
 * frame with the callee's incoming arguments only, so on resumption the
 * for-each iterator read back null:
 *
 *   NullPointerException: Cannot invoke "java.util.Iterator.hasNext()"
 *                         because "<local5>" is null
 *
 * The call chain matters: `drive` must be COMPILED so its call to `convert`
 * goes through `jit_invoke_dispatch`, which is what routes the escaping
 * exception into `run_jit_callee_handler` instead of the interpreter's own
 * sink. Prints one line; a non-zero mismatch count (or an NPE) is a failure.
 */
public class JitHandlerResumeLocalProbe {

	static final class Boom extends RuntimeException {
		Boom(String m) {
			super(m);
		}
	}

	interface Svc {
		int run(int x);
	}

	static final class Plain implements Svc {
		private final int tag;

		Plain(int tag) {
			this.tag = tag;
		}

		@Override
		public int run(int x) {
			return x + this.tag;
		}
	}

	static final class Thrower implements Svc {
		@Override
		public int run(int x) {
			return deep(x, 3);
		}

		private static int deep(int x, int depth) {
			if (depth == 0) {
				throw new Boom("depth " + x);
			}
			return deep(x, depth - 1);
		}
	}

	private final List<Svc> delegates;

	JitHandlerResumeLocalProbe(List<Svc> raw) {
		this.delegates = Collections.unmodifiableList(raw);
	}

	/**
	 * Locals: 0 this, 1 source, 2 caught (pre-try), 3 iterator (pre-try),
	 * 4 delegate, 5 ex. The handler reads local 2, which is what admits this
	 * method through the relaxed RBC.6 gate; the loop back-edge then reads
	 * local 3, which is what the params-only reconstruction zeroed.
	 */
	int convert(int source) {
		int caught = 0;
		for (Svc delegate : this.delegates) {
			try {
				// Only invoke opcodes may appear inside the protected range:
				// `precise_exception_frame_sites_supported` refuses a range
				// containing an `ldc` (or a field/array/alloc/cast op), and the
				// method then stays interpreted — which silently makes this
				// probe a false null. Keep the body to the call plus
				// non-throwing arithmetic.
				caught = caught + delegate.run(source) - delegate.run(source);
			}
			catch (Boom ex) {
				caught = caught + 1;
			}
		}
		return caught;
	}

	private int drive(int n) {
		int mismatches = 0;
		for (int i = 0; i < n; i++) {
			if (convert(i) != 2) {
				mismatches++;
			}
		}
		return mismatches;
	}

	public static void main(String[] args) {
		int n = (args.length > 0) ? Integer.parseInt(args[0]) : 200000;
		List<Svc> raw = new ArrayList<>();
		raw.add(new Plain(1));
		raw.add(new Thrower());
		raw.add(new Plain(2));
		raw.add(new Thrower());
		JitHandlerResumeLocalProbe p = new JitHandlerResumeLocalProbe(raw);
		int mismatches = 0;
		for (int round = 0; round < 8; round++) {
			mismatches += p.drive(n / 8);
		}
		System.out.println("JIT_HANDLER_RESUME_LOCAL_PROBE mismatches=" + mismatches);
	}
}
