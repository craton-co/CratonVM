import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Times {@code get}/{@code put} on {@link HashMap} and {@link LinkedHashMap}
 * keyed by an object whose {@code hashCode()} is a plain int field — the exact
 * shape Hibernate's {@code QueryParameterBindingsImpl.parameterBindingMap}
 * uses (a 100k-entry {@code LinkedHashMap<QueryParameterIdentifiedImpl, ...>}
 * whose key hashes to {@code unnamedParameterId}).
 *
 * Four arms, because the three plausible pathologies are told apart only by
 * which arm degrades: default-capacity (does the table resize?), presized
 * (is an explicit initial capacity honoured?), and all-colliding hashes (does
 * one chain degrade to a linear walk?).
 *
 * Prints ns/op at a ladder of sizes so an O(size) lookup shows up as a shape,
 * not as one unattributable data point. Run it on the host JDK first: the
 * numbers only mean something against that control.
 */
public final class MapGetKeyedProbe {
	private MapGetKeyedProbe() {}

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

	/** Same identity contract, but every instance hashes to the same bucket. */
	static final class ClashKey {
		final int id;

		ClashKey(int id) {
			this.id = id;
		}

		@Override
		public boolean equals(Object o) {
			return this == o || (o instanceof ClashKey && ((ClashKey) o).id == id);
		}

		@Override
		public int hashCode() {
			return 7;
		}
	}

	static void run(String label, Map<Object, Object> map, boolean clash, int n, int probes) {
		long tPut0 = System.nanoTime();
		for (int i = 0; i < n; i++) {
			map.put(clash ? new ClashKey(i) : new Key(i), Integer.valueOf(i));
		}
		long tPut = System.nanoTime() - tPut0;

		Object sink = null;
		long tGet0 = System.nanoTime();
		for (int i = 0; i < probes; i++) {
			int id = i % n;
			sink = map.get(clash ? new ClashKey(id) : new Key(id));
		}
		long tGet = System.nanoTime() - tGet0;
		if (sink == null) {
			throw new IllegalStateException("lookup missed — probe would be vacuous");
		}
		System.out.printf("@@MAP %-22s n=%-6d put_ns_per_op=%-9d get_ns_per_op=%-9d put_ms=%-6d get_ms=%d%n",
				label, n, tPut / n, tGet / probes, tPut / 1_000_000L, tGet / 1_000_000L);
		System.out.flush();
	}

	public static void main(String[] args) {
		int[] sizes = { 5000, 20000 };
		int probes = 2000;
		if (args.length > 0) {
			String[] parts = args[0].split(",");
			sizes = new int[parts.length];
			for (int i = 0; i < parts.length; i++) {
				sizes[i] = Integer.parseInt(parts[i].trim());
			}
		}
		if (args.length > 1) {
			probes = Integer.parseInt(args[1]);
		}
		for (int n : sizes) {
			run("HashMap/grow", new HashMap<>(), false, n, probes);
			run("LinkedHashMap/grow", new LinkedHashMap<>(), false, n, probes);
			run("LinkedHashMap/presized", new LinkedHashMap<>(n), false, n, probes);
			run("LinkedHashMap/clash", new LinkedHashMap<>(), true, n, probes);
		}
		System.out.println("@@PROBEEND ok");
	}
}
