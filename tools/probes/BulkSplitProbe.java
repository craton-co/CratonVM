import java.nio.ByteBuffer;
import java.util.Locale;

/** Decomposition: which of the two calls in the bulk loop body costs what.
 *  position(0) and get(byte[]) are BOTH registered bridge natives, so the
 *  per-call funnel is paid twice per iteration and the split says whether the
 *  copy or the crossing dominates. */
public final class BulkSplitProbe {
	public static void main(String[] args) {
		int n = args.length > 0 ? Integer.parseInt(args[0]) : 300000;
		int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 4;
		ByteBuffer heap = ByteBuffer.allocate(65536);
		ByteBuffer direct = ByteBuffer.allocateDirect(65536);
		byte[] a16 = new byte[16];
		byte[] a256 = new byte[256];
		byte[] a4096 = new byte[4096];
		for (int r = 0; r < rounds; r++) {
			boolean rec = r == rounds - 1;
			time("position(0) only", n, rec, () -> posOnly(heap, n));
			time("limit() only", n, rec, () -> limitOnly(heap, n));
			time("heap get(byte[16])   +pos", n, rec, () -> bulkGet(heap, a16, n));
			time("heap get(byte[256])  +pos", n, rec, () -> bulkGet(heap, a256, n));
			time("heap get(byte[4096]) +pos", n, rec, () -> bulkGet(heap, a4096, n));
			time("heap get(byte[256]) nopos", n, rec, () -> bulkGetNoPos(heap, a256, n));
			time("direct get(byte[256])+pos", n, rec, () -> bulkGet(direct, a256, n));
			time("direct put(byte[256])+pos", n, rec, () -> bulkPut(direct, a256, n));
			time("arraycopy(byte[256])", n, rec, () -> ac(a256, new byte[256], n));
		}
	}

	private static long posOnly(ByteBuffer b, int n) {
		for (int i = 0; i < n; i++) b.position(0);
		return b.position();
	}
	private static long limitOnly(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) s += b.limit();
		return s;
	}
	private static long bulkGet(ByteBuffer b, byte[] sink, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) { b.position(0); b.get(sink); s += sink[0]; }
		return s;
	}
	private static long bulkGetNoPos(ByteBuffer b, byte[] sink, int n) {
		long s = 0;
		b.position(0);
		for (int i = 0; i < n; i++) { b.get(sink); b.position(b.position() - sink.length); s += sink[0]; }
		return s;
	}
	private static long bulkPut(ByteBuffer b, byte[] src, int n) {
		for (int i = 0; i < n; i++) { b.position(0); b.put(src); }
		return b.get(0);
	}
	private static long ac(byte[] src, byte[] dst, int n) {
		for (int i = 0; i < n; i++) System.arraycopy(src, 0, dst, 0, src.length);
		return dst[0];
	}

	interface Body { long run(); }

	private static void time(String name, int iters, boolean record, Body body) {
		long t0 = System.nanoTime();
		long sink = body.run();
		long dt = System.nanoTime() - t0;
		if (record) System.out.printf(Locale.ROOT, "%-28s %8.2f ns/op   (%d)%n", name, (double) dt / iters, sink & 1);
	}
}
