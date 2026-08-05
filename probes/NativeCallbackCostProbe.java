import java.util.HashMap;
import java.util.Map;

/**
 * Prices the native → Java call-back edge.
 *
 * CratonVM implements {@code java.util.HashMap} as a Rust native. A lookup by
 * a key whose {@code hashCode}/{@code equals} are Java bytecode therefore has
 * to re-enter the interpreter twice; a lookup by {@code String} does not (the
 * native hashes and compares the text itself). Subtracting the two, with a
 * plain Java virtual call as the third leg, gives the cost of ONE
 * native → Java re-entry — the edge every dispatch-heavy Hibernate workload
 * crosses millions of times.
 *
 * Run it on the host JDK first: only the ratio against that control means
 * anything.
 */
public final class NativeCallbackCostProbe {
	private NativeCallbackCostProbe() {}

	static final class Key {
		final int id;

		Key(int id) {
			this.id = id;
		}

		@Override
		public boolean equals(Object o) {
			return this == o || (o instanceof Key && ((Key) o).id == id);
		}

		@Override
		public int hashCode() {
			return id;
		}
	}

	static int sink;

	public static void main(String[] args) {
		int n = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
		int iters = args.length > 1 ? Integer.parseInt(args[1]) : 20000;

		Map<String, Object> byString = new HashMap<>();
		Map<Key, Object> byKey = new HashMap<>();
		Map<Object, Object> byIdentity = new HashMap<>();
		String[] strings = new String[n];
		Key[] keys = new Key[n];
		Object[] plain = new Object[n];
		for (int i = 0; i < n; i++) {
			strings[i] = "k" + i;
			keys[i] = new Key(i);
			plain[i] = new Object();
			byString.put(strings[i], Integer.valueOf(i));
			byKey.put(keys[i], Integer.valueOf(i));
			byIdentity.put(plain[i], Integer.valueOf(i));
		}

		for (int pass = 0; pass < 2; pass++) {
			Object s = null;
			long t0 = System.nanoTime();
			for (int i = 0; i < iters; i++) {
				s = byString.get(strings[i % n]);
			}
			long tString = System.nanoTime() - t0;

			t0 = System.nanoTime();
			for (int i = 0; i < iters; i++) {
				s = byKey.get(keys[i % n]);
			}
			long tKey = System.nanoTime() - t0;

			t0 = System.nanoTime();
			for (int i = 0; i < iters; i++) {
				s = byIdentity.get(plain[i % n]);
			}
			long tIdentity = System.nanoTime() - t0;

			t0 = System.nanoTime();
			for (int i = 0; i < iters; i++) {
				sink += keys[i % n].hashCode();
			}
			long tCall = System.nanoTime() - t0;

			if (s == null) {
				throw new IllegalStateException("lookup missed — probe would be vacuous");
			}
			System.out.printf(
					"@@CB pass=%d n=%d string_get_ns=%d key_get_ns=%d identity_get_ns=%d java_call_ns=%d%n",
					pass, n, tString / iters, tKey / iters, tIdentity / iters, tCall / iters);
			System.out.flush();
		}
		System.out.println("@@PROBEEND ok sink=" + sink);
	}
}
