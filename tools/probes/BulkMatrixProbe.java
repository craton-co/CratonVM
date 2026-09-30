import java.nio.ByteBuffer;
import java.util.Arrays;

/**
 * Differential oracle for the BULK arms, which the scalar accessor matrix does
 * not reach at all. Both changes under test here are invisible-by-construction
 * (a memo of a pure lookup; a reused staging buffer), and "invisible by
 * construction" is precisely the claim that needs an oracle rather than an
 * argument -- a reused buffer that is under-filled shows up as STALE BYTES
 * from the previous call, which only a varying-length, varying-content
 * sequence can catch.
 */
public final class BulkMatrixProbe {
	public static void main(String[] a) {
		for (boolean direct : new boolean[] { false, true }) {
			ByteBuffer b = direct ? ByteBuffer.allocateDirect(4096) : ByteBuffer.allocate(4096);
			for (int i = 0; i < b.capacity(); i++) b.put(i, (byte) (i * 31 + (direct ? 7 : 0)));
			String k = direct ? "direct" : "heap";

			// Descending then ascending lengths: a scratch buffer that is not
			// re-filled to the CURRENT length leaks the previous call's tail.
			for (int len : new int[] { 300, 4, 128, 1, 257, 16, 1024, 3, 512 }) {
				byte[] dst = new byte[len];
				b.position(0);
				b.get(dst);
				System.out.println(k + " get(" + len + ") = " + sum(dst) + " " + head(dst) + " pos=" + b.position());

				byte[] dst2 = new byte[len + 8];
				Arrays.fill(dst2, (byte) 0xAB);
				b.position(9);
				b.get(dst2, 4, len);
				System.out.println(k + " get(off,"+len+") = " + sum(dst2) + " " + head(dst2) + " pos=" + b.position());

				byte[] src = new byte[len];
				for (int i = 0; i < len; i++) src[i] = (byte) (i ^ len);
				b.position(3);
				b.put(src);
				System.out.println(k + " put(" + len + ") -> " + sum(all(b)) + " pos=" + b.position());

				if (len >= 2) {
					b.position(1);
					b.put(src, 2, len - 2);
					System.out.println(k + " put(off," + len + ") -> " + sum(all(b)) + " pos=" + b.position());
				}
			}

			// buffer-to-buffer, which routes through the same staging path
			ByteBuffer other = direct ? ByteBuffer.allocate(2048) : ByteBuffer.allocateDirect(2048);
			for (int i = 0; i < other.capacity(); i++) other.put(i, (byte) (i * 17));
			for (int len : new int[] { 700, 5, 300 }) {
				other.position(0).limit(len);
				b.position(2);
				b.put(other);
				System.out.println(k + " put(buf," + len + ") -> " + sum(all(b)) + " pos=" + b.position());
				other.clear();
			}

			// exceptions must keep their class and message
			for (int len : new int[] { 5000, -1 }) {
				try {
					b.position(0);
					b.get(new byte[Math.max(0, len)]);
					System.out.println(k + " get(" + len + ") no-throw");
				} catch (Throwable t) {
					System.out.println(k + " get(" + len + ") threw " + t.getClass().getName() + ": " + t.getMessage());
				}
			}
			try {
				b.position(0);
				b.get(new byte[16], 12, 10);
			} catch (Throwable t) {
				System.out.println(k + " get(bad off/len) threw " + t.getClass().getName() + ": " + t.getMessage());
			}
		}
	}

	private static byte[] all(ByteBuffer b) {
		byte[] o = new byte[b.capacity()];
		for (int i = 0; i < o.length; i++) o[i] = b.get(i);
		return o;
	}
	private static long sum(byte[] x) {
		long s = 0;
		for (int i = 0; i < x.length; i++) s += (long) (x[i] & 0xFF) * (i + 1);
		return s;
	}
	private static String head(byte[] x) {
		StringBuilder sb = new StringBuilder("[");
		for (int i = 0; i < Math.min(6, x.length); i++) sb.append(x[i] & 0xFF).append(',');
		return sb.append("..]").toString();
	}
}
