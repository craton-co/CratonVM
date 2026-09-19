import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * Prices every `java.nio.ByteBuffer` scalar and bulk operation an HTTP codec
 * actually executes, across the two buffer shapes that matter: heap-backed
 * (`byte[]`, what `ByteBuffer.allocate` gives you) and direct (off-heap, what
 * every `SocketChannel` read/write ends up touching).
 *
 * Why both shapes: they take completely different paths under this VM.
 * `HeapByteBuffer` bottoms out in `Unsafe.get*Unaligned(byte[], long)`, which
 * the runtime services with the `*_mb` handlers over the Java heap; direct
 * buffers bottom out in `Unsafe.get*(long)` over native memory. A fix aimed at
 * one shape can leave the other untouched, so a single number for "ByteBuffer"
 * hides which one is actually slow.
 *
 * Every arm is bracketed by a control arm of the same iteration count that
 * touches only locals, so the loop overhead is visible rather than folded in.
 *
 * Usage: `NioBufferCostProbe [ops] [rounds]` — reports the MINIMUM ns/op across
 * rounds, arms interleaved, which is the statistic that survives a loaded host.
 */
public final class NioBufferCostProbe {

	private static final int CAPACITY = 64 * 1024;
	private static final int MASK = CAPACITY - 16;

	private static int ops = 5_000_000;
	private static int rounds = 3;

	/** Arm name -> best ns/op seen so far. */
	private static final java.util.LinkedHashMap<String, Double> best = new java.util.LinkedHashMap<>();

	public static void main(String[] args) {
		if (args.length > 0) {
			ops = Integer.parseInt(args[0]);
		}
		if (args.length > 1) {
			rounds = Integer.parseInt(args[1]);
		}

		ByteBuffer heap = fill(ByteBuffer.allocate(CAPACITY)).order(ByteOrder.LITTLE_ENDIAN);
		ByteBuffer direct = fill(ByteBuffer.allocateDirect(CAPACITY)).order(ByteOrder.LITTLE_ENDIAN);
		ByteBuffer heapBe = fill(ByteBuffer.allocate(CAPACITY)).order(ByteOrder.BIG_ENDIAN);
		byte[] raw = new byte[CAPACITY];
		heap.get(0, raw);
		byte[] sink = new byte[256];

		// One untimed pass of every arm, so nothing below measures a cold tier.
		round(heap, direct, heapBe, raw, sink, Math.min(ops, 200_000), false);

		for (int r = 0; r < rounds; r++) {
			round(heap, direct, heapBe, raw, sink, ops, true);
		}

		System.out.println("arm,ns_per_op");
		for (java.util.Map.Entry<String, Double> e : best.entrySet()) {
			System.out.printf(java.util.Locale.ROOT, "%-28s,%8.2f%n", e.getKey(), e.getValue());
		}
	}

	private static void round(ByteBuffer heap, ByteBuffer direct, ByteBuffer heapBe, byte[] raw,
			byte[] sink, int n, boolean record) {
		time("control (locals only)", n, record, () -> control(n));
		time("raw byte[] get", n, record, () -> rawGet(raw, n));

		time("heap get(int)", n, record, () -> absGet(heap, n));
		time("heap getShort(int)", n, record, () -> absGetShort(heap, n));
		time("heap getInt(int)", n, record, () -> absGetInt(heap, n));
		time("heap getLong(int)", n, record, () -> absGetLong(heap, n));
		time("heap getInt(int) BE", n, record, () -> absGetInt(heapBe, n));
		time("heap getShort() rel", n, record, () -> relGetShort(heap, n));
		time("heap getInt() rel", n, record, () -> relGetInt(heap, n));
		time("heap putInt(int,int)", n, record, () -> absPutInt(heap, n));
		time("heap putLong(int,long)", n, record, () -> absPutLong(heap, n));

		time("direct get(int)", n, record, () -> absGet(direct, n));
		time("direct getShort(int)", n, record, () -> absGetShort(direct, n));
		time("direct getInt(int)", n, record, () -> absGetInt(direct, n));
		time("direct getLong(int)", n, record, () -> absGetLong(direct, n));
		time("direct getInt() rel", n, record, () -> relGetInt(direct, n));
		time("direct putInt(int,int)", n, record, () -> absPutInt(direct, n));

		// Bulk arms run 1/64th the iterations: each moves 256 bytes.
		int bulkN = Math.max(1, n / 64);
		time("heap get(byte[256])", bulkN, record, () -> bulkGet(heap, sink, bulkN));
		time("direct get(byte[256])", bulkN, record, () -> bulkGet(direct, sink, bulkN));
		time("heap put(byte[256])", bulkN, record, () -> bulkPut(heap, sink, bulkN));
		time("direct put(byte[256])", bulkN, record, () -> bulkPut(direct, sink, bulkN));
	}

	// ---- arms -------------------------------------------------------------

	private static long control(int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			s += (i * 7) & MASK;
		}
		return s;
	}

	private static long rawGet(byte[] a, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			s += a[(i * 7) & MASK];
		}
		return s;
	}

	private static long absGet(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			s += b.get((i * 7) & MASK);
		}
		return s;
	}

	private static long absGetShort(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			s += b.getShort((i * 7) & MASK);
		}
		return s;
	}

	private static long absGetInt(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			s += b.getInt((i * 7) & MASK);
		}
		return s;
	}

	private static long absGetLong(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			s += b.getLong((i * 7) & MASK);
		}
		return s;
	}

	private static long absPutInt(ByteBuffer b, int n) {
		for (int i = 0; i < n; i++) {
			b.putInt((i * 7) & MASK, i);
		}
		return b.getInt(0);
	}

	private static long absPutLong(ByteBuffer b, int n) {
		for (int i = 0; i < n; i++) {
			b.putLong((i * 7) & MASK, i);
		}
		return b.getLong(0);
	}

	/**
	 * The relative forms — what a header parser actually calls. `position(0)`
	 * every 1024 reads, not every read: the rewind is itself a call and paying
	 * it per iteration would bury the thing being measured.
	 */
	private static long relGetShort(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			if ((i & 1023) == 0) {
				b.position(0);
			}
			s += b.getShort();
		}
		return s;
	}

	private static long relGetInt(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			if ((i & 511) == 0) {
				b.position(0);
			}
			s += b.getInt();
		}
		return s;
	}

	private static long bulkGet(ByteBuffer b, byte[] sink, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) {
			b.position(0);
			b.get(sink);
			s += sink[0];
		}
		return s;
	}

	private static long bulkPut(ByteBuffer b, byte[] src, int n) {
		for (int i = 0; i < n; i++) {
			b.position(0);
			b.put(src);
		}
		return b.get(0);
	}

	// ---- harness ----------------------------------------------------------

	private static ByteBuffer fill(ByteBuffer b) {
		for (int i = 0; i < b.capacity(); i++) {
			b.put(i, (byte) i);
		}
		return b;
	}

	private static void time(String arm, int n, boolean record, java.util.function.LongSupplier body) {
		long start = System.nanoTime();
		long sink = body.getAsLong();
		long elapsed = System.nanoTime() - start;
		if (sink == Long.MIN_VALUE) {
			System.out.println("unreachable");
		}
		if (record) {
			double per = (double) elapsed / n;
			best.merge(arm, per, Math::min);
		}
	}

}
