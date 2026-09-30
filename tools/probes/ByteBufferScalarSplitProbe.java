import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * Splits the `ByteBuffer` scalar-read cost by layer.
 *
 * `HeapByteBuffer.get(int)` reads `hb[ix(checkIndex(i))]` straight out of the
 * backing array — ordinary Java, no native. `getShort(int)`/`getInt(int)` go
 * through `SCOPED_MEMORY_ACCESS.get*Unaligned`, which is a registered native
 * here. If `get` is cheap and `getShort` is not, the cost is that crossing and
 * not the surrounding index arithmetic.
 *
 * Also prices a hand-rolled little-endian assembly from single-byte reads,
 * which is what the caller would write if the accessor were the problem.
 */
public final class ByteBufferScalarSplitProbe {

	private static final int OPS = 5_000_000;

	public static void main(String[] args) {
		ByteBuffer buffer = ByteBuffer.allocate(64 * 1024).order(ByteOrder.LITTLE_ENDIAN);
		for (int i = 0; i < buffer.capacity(); i++) {
			buffer.put(i, (byte) i);
		}
		byte[] array = new byte[buffer.capacity()];
		buffer.get(0, array);

		// Warm-up for every arm.
		singleByte(buffer, 100_000);
		getShort(buffer, 100_000);
		getInt(buffer, 100_000);
		handRolled(buffer, 100_000);
		rawArray(array, 100_000);

		relativeShort(buffer, 100_000);
		relativeInt(buffer, 100_000);

		System.out.println("arm,ops,millis,ns_per_op");
		time("buffer.get(int)      ", () -> singleByte(buffer, OPS));
		time("buffer.getShort(int) ", () -> getShort(buffer, OPS));
		time("buffer.getInt(int)   ", () -> getInt(buffer, OPS));
		time("buffer.getShort()    ", () -> relativeShort(buffer, OPS));
		time("buffer.getInt()      ", () -> relativeInt(buffer, OPS));
		time("hand-rolled LE short ", () -> handRolled(buffer, OPS));
		time("raw byte[] read      ", () -> rawArray(array, OPS));
	}

	/**
	 * The RELATIVE forms, which is what Spring Boot's zip header reader
	 * actually calls — eleven `getShort()` and six `getInt()` per central
	 * directory record, and the absolute forms never.
	 *
	 * `position(0)` every 1024 reads rather than every read: rewinding is
	 * itself a call, and paying it per iteration would bury the thing being
	 * measured.
	 */
	private static long relativeShort(ByteBuffer buffer, int ops) {
		long sink = 0;
		for (int i = 0; i < ops; i++) {
			if ((i & 1023) == 0) {
				buffer.position(0);
			}
			sink += buffer.getShort();
		}
		return sink;
	}

	private static long relativeInt(ByteBuffer buffer, int ops) {
		long sink = 0;
		for (int i = 0; i < ops; i++) {
			if ((i & 1023) == 0) {
				buffer.position(0);
			}
			sink += buffer.getInt();
		}
		return sink;
	}

	private static long singleByte(ByteBuffer buffer, int ops) {
		long sink = 0;
		int mask = buffer.capacity() - 8;
		for (int i = 0; i < ops; i++) {
			sink += buffer.get((i * 7) & mask);
		}
		return sink;
	}

	private static long getShort(ByteBuffer buffer, int ops) {
		long sink = 0;
		int mask = buffer.capacity() - 8;
		for (int i = 0; i < ops; i++) {
			sink += buffer.getShort((i * 7) & mask);
		}
		return sink;
	}

	private static long getInt(ByteBuffer buffer, int ops) {
		long sink = 0;
		int mask = buffer.capacity() - 8;
		for (int i = 0; i < ops; i++) {
			sink += buffer.getInt((i * 7) & mask);
		}
		return sink;
	}

	private static long handRolled(ByteBuffer buffer, int ops) {
		long sink = 0;
		int mask = buffer.capacity() - 8;
		for (int i = 0; i < ops; i++) {
			int at = (i * 7) & mask;
			sink += (short) ((buffer.get(at) & 0xFF) | (buffer.get(at + 1) << 8));
		}
		return sink;
	}

	private static long rawArray(byte[] array, int ops) {
		long sink = 0;
		int mask = array.length - 8;
		for (int i = 0; i < ops; i++) {
			sink += array[(i * 7) & mask];
		}
		return sink;
	}

	private static void time(String arm, java.util.function.LongSupplier body) {
		long start = System.nanoTime();
		long sink = body.getAsLong();
		long elapsed = System.nanoTime() - start;
		System.out.printf("%s,%d,%d,%.2f%s%n", arm, OPS, elapsed / 1_000_000,
				(double) elapsed / OPS, (sink == Long.MIN_VALUE) ? " (unreachable)" : "");
	}

}
