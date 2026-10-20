import java.nio.ByteBuffer;
import java.util.Locale;

/**
 * The bulk arms only, at several lengths. Separated from NioBufferCostProbe so
 * a change to the bulk path can be A/B'd without paying for the scalar matrix,
 * and so the LENGTH sweep is visible: a fixed per-call overhead and a per-byte
 * cost look identical at one length and nothing alike across four.
 */
public final class BulkCostProbe {
	public static void main(String[] args) {
		int n = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
		int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
		ByteBuffer heap = ByteBuffer.allocate(65536);
		ByteBuffer direct = ByteBuffer.allocateDirect(65536);
		for (int r = 0; r < rounds; r++) {
			boolean rec = r == rounds - 1;
			for (int len : new int[] { 16, 256, 4096, 65536 }) {
				byte[] a = new byte[len];
				int iters = Math.max(1, n / Math.max(1, len / 16));
				time("heap get(byte[" + len + "])", iters, rec, () -> bulkGet(heap, a, iters));
				time("direct get(byte[" + len + "])", iters, rec, () -> bulkGet(direct, a, iters));
				time("heap put(byte[" + len + "])", iters, rec, () -> bulkPut(heap, a, iters));
				time("direct put(byte[" + len + "])", iters, rec, () -> bulkPut(direct, a, iters));
			}
		}
	}

	private static long bulkGet(ByteBuffer b, byte[] sink, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) { b.position(0); b.get(sink); s += sink[0]; }
		return s;
	}

	private static long bulkPut(ByteBuffer b, byte[] src, int n) {
		for (int i = 0; i < n; i++) { b.position(0); b.put(src); }
		return b.get(0);
	}

	interface Body { long run(); }

	private static void time(String name, int iters, boolean record, Body body) {
		long t0 = System.nanoTime();
		long sink = body.run();
		long dt = System.nanoTime() - t0;
		if (record) {
			System.out.printf(Locale.ROOT, "%-28s %8.2f ns/op   (%d)%n", name, (double) dt / iters, sink & 1);
		}
	}
}
