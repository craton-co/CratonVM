import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * The DIRECT-receiver twin of `HeapBufferShapeControlProbe`.
 *
 * The thin `ByteBuffer.get(int)` element helper exists for this shape and only
 * this shape, so any change to when that helper is BOUND has to be measured
 * here as well: a gate that fixes the heap case by refusing the bind
 * everywhere would show up as a regression on this probe and nowhere else.
 *
 * `armDirect` is never handed anything but a direct buffer, so its call site
 * is monomorphic and its receiver profile is unambiguous — which is exactly
 * the input the bind gate reads.
 */
public final class DirectBufferShapeControlProbe {
	static int N = 1_000_000;

	public static void main(String[] a) {
		if (a.length > 0) N = Integer.parseInt(a[0]);
		ByteBuffer direct = ByteBuffer.allocateDirect(65536).order(ByteOrder.LITTLE_ENDIAN);
		for (int i = 0; i < 65536; i++) direct.put(i, (byte) i);
		byte[] raw = new byte[65536];

		for (int r = 0; r < 4; r++) {
			long t;
			t = System.nanoTime(); long x = armDirect(direct, N); p("DirectByteBuffer.get", t, x);
			t = System.nanoTime(); long z = armRaw(raw, N);        p("raw byte[]          ", t, z);
			System.out.println("--");
		}
	}

	static void p(String s, long t0, long sink) {
		long e = System.nanoTime() - t0;
		System.out.printf(java.util.Locale.ROOT, "%s %8.2f ns/op%s%n", s, (double) e / N,
				sink == Long.MIN_VALUE ? "!" : "");
	}

	static long armDirect(ByteBuffer b, int n) {
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
