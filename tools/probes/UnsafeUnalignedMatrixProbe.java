import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.Arrays;

/**
 * Differential oracle for the `Unsafe.*Unaligned` multi-byte accessors, at the
 * two layers that reach them.
 *
 * Layer 1 — `ByteBuffer` over a `byte[]`: the one-byte-per-element case, which
 * is what every `HeapByteBuffer` scalar accessor takes.
 *
 * Layer 2 — reads that SPAN several elements of a wider primitive array
 * (`char[]`, `int[]`, `long[]`). Nothing in the public API reaches these
 * directly; `jdk.internal.util.ArraysSupport.vectorizedMismatch` does, and
 * `Arrays.equals(char[]/int[]/long[])` and `String.equals` sit on top of it.
 * A word read that silently truncated to its first element broke exactly
 * those, so they are exercised here by comparing arrays that differ only in a
 * late element — the shape a truncating read reports as equal.
 *
 * Prints one line per case. Diff the whole output against HotSpot's: any
 * difference is a defect, and the exception CLASS and MESSAGE are part of the
 * contract, not just the values.
 */
public final class UnsafeUnalignedMatrixProbe {

	public static void main(String[] args) {
		byteArrayLayer();
		spanningLayer();
		mismatchLayer();
	}

	// ---- layer 1: byte[]-backed ByteBuffer, every width and both orders ----

	private static void byteArrayLayer() {
		for (ByteOrder order : new ByteOrder[] { ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN }) {
			for (boolean direct : new boolean[] { false, true }) {
				ByteBuffer b = direct ? ByteBuffer.allocateDirect(64) : ByteBuffer.allocate(64);
				b.order(order);
				for (int i = 0; i < 64; i++) {
					b.put(i, (byte) (i * 7 + 1));
				}
				String tag = (direct ? "direct/" : "heap/") + order;
				// Every alignment within a word, so an offset-dependent bug shows.
				for (int off = 0; off < 8; off++) {
					System.out.printf("%s off=%d short=%d char=%d int=%d long=%d%n", tag, off,
							b.getShort(off), (int) b.getChar(off), b.getInt(off), b.getLong(off));
				}
				// Round-trip: what was written must read back, at every alignment.
				for (int off = 0; off < 8; off++) {
					b.putShort(off, (short) 0x1234);
					b.putInt(off + 8, 0x0A0B0C0D);
					b.putLong(off + 16, 0x0102030405060708L);
					System.out.printf("%s rt off=%d short=%d int=%d long=%d%n", tag, off,
							b.getShort(off), b.getInt(off + 8), b.getLong(off + 16));
				}
				// Float/double travel as bit patterns; NaN and -0.0 are the two
				// that a value-level round trip would lose.
				b.putFloat(0, Float.NaN);
				b.putDouble(8, -0.0d);
				System.out.printf("%s nan=%d negzero=%d%n", tag,
						Float.floatToRawIntBits(b.getFloat(0)),
						Double.doubleToRawLongBits(b.getDouble(8)));
				// Bounds: the class and the (null) message are the contract.
				b.limit(20);
				try {
					b.getLong(15);
					System.out.printf("%s past-limit: NO THROW%n", tag);
				} catch (RuntimeException e) {
					System.out.printf("%s past-limit -> %s: %s%n", tag,
							e.getClass().getName(), e.getMessage());
				}
			}
		}
	}

	// ---- layer 2: multi-byte reads that span wider elements ----------------

	/**
	 * `Arrays.equals` over the wide primitive types routes through
	 * `ArraysSupport.vectorizedMismatch`, i.e. through `getLongUnaligned` over
	 * a `char[]`/`int[]`/`long[]`. Each pair below differs in ONE element, at a
	 * position chosen so a read that truncated to its first element would
	 * report the pair equal.
	 */
	private static void spanningLayer() {
		for (int len : new int[] { 1, 2, 3, 4, 7, 8, 9, 16, 33 }) {
			for (int at = 0; at < len; at++) {
				char[] ca = new char[len];
				char[] cb = new char[len];
				int[] ia = new int[len];
				int[] ib = new int[len];
				long[] la = new long[len];
				long[] lb = new long[len];
				for (int i = 0; i < len; i++) {
					ca[i] = cb[i] = (char) (i + 'a');
					ia[i] = ib[i] = i * 1_000_003;
					la[i] = lb[i] = i * 1_000_000_007L;
				}
				cb[at] ^= 0x0100;
				ib[at] ^= 0x0001_0000;
				lb[at] ^= 0x0000_0001_0000_0000L;
				boolean bad = Arrays.equals(ca, cb) || Arrays.equals(ia, ib) || Arrays.equals(la, lb);
				if (bad) {
					System.out.printf("SPAN FAIL len=%d at=%d char=%b int=%b long=%b%n", len, at,
							Arrays.equals(ca, cb), Arrays.equals(ia, ib), Arrays.equals(la, lb));
				}
			}
		}
		System.out.println("spanning: differing arrays never compare equal — OK");
	}

	/** `String.equals` is the same machinery over `byte[]`, and the case the
	 *  original truncation bug was found on ("Signature" vs "Synthetic"). */
	private static void mismatchLayer() {
		String[] words = { "Signature", "Synthetic", "SourceFile", "SourceDebugExtension",
				"StackMapTable", "StackMap", "", "a", "ab" };
		int equalPairs = 0;
		for (String x : words) {
			for (String y : words) {
				if (x.equals(y)) {
					equalPairs++;
				}
			}
		}
		System.out.println("string equal pairs (expect " + words.length + "): " + equalPairs);
		System.out.println("mismatch: " + Arrays.mismatch("Signature".getBytes(),
				"Synthetic".getBytes()));
	}

}
