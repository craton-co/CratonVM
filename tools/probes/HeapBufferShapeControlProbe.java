import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * A hand-written clone of HeapByteBuffer.get(int)'s exact shape, in the
 * application class loader. Same abstract-base + concrete-subclass structure,
 * same three-call chain (get -> checkIndex -> ix -> array load).
 *
 * If MyBuf is fast and HeapByteBuffer is slow, the cost is something about
 * java.nio / the JDK class source, not about the code shape.
 */
public final class HeapBufferShapeControlProbe {
	static int N = 1_000_000;

	abstract static class AbstractBuf {
		int limit;
		abstract byte get(int i);
	}

	static class MyBuf extends AbstractBuf {
		final byte[] hb;
		final int offset;

		MyBuf(int cap) { hb = new byte[cap]; offset = 0; limit = cap; }

		final int checkIndex(int i) {
			if (i < 0 || i >= limit) throw new IndexOutOfBoundsException();
			return i;
		}

		final int ix(int i) { return i + offset; }

		@Override byte get(int i) { return hb[ix(checkIndex(i))]; }
	}

	public static void main(String[] a) {
		if (a.length > 0) N = Integer.parseInt(a[0]);
		AbstractBuf mine = new MyBuf(65536);
		ByteBuffer jdk = ByteBuffer.allocate(65536).order(ByteOrder.LITTLE_ENDIAN);
		byte[] raw = new byte[65536];

		for (int r = 0; r < 4; r++) {
			long t;
			t = System.nanoTime(); long x = armMine(mine, N);   p("MyBuf.get(int)      ", t, x);
			t = System.nanoTime(); long y = armJdk(jdk, N);     p("HeapByteBuffer.get  ", t, y);
			t = System.nanoTime(); long z = armRaw(raw, N);     p("raw byte[]          ", t, z);
			System.out.println("--");
		}
	}

	static void p(String s, long t0, long sink) {
		long e = System.nanoTime() - t0;
		System.out.printf(java.util.Locale.ROOT, "%s %8.2f ns/op%s%n", s, (double) e / N,
				sink == Long.MIN_VALUE ? "!" : "");
	}

	static long armMine(AbstractBuf b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) s += b.get((i * 7) & 65520);
		return s;
	}

	static long armJdk(ByteBuffer b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) s += b.get((i * 7) & 65520);
		return s;
	}

	static long armRaw(byte[] b, int n) {
		long s = 0;
		for (int i = 0; i < n; i++) s += b[(i * 7) & 65520];
		return s;
	}
}
