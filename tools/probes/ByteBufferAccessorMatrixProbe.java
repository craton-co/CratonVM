import java.nio.Buffer;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * The contract an intrinsified `HeapByteBuffer` scalar accessor has to keep,
 * printed per shape so it can be diffed line-for-line against a HotSpot
 * control rather than reasoned about.
 *
 * The shapes that matter, and why each is here:
 *   allocate / wrap / wrap(off,len)  — `offset` is 0, `position` may not be
 *   slice / duplicate / slice(i,n)   — non-zero `offset` field, which any
 *                                      backing-array read must add in
 *   asReadOnlyBuffer                 — `HeapByteBufferR`, a SUBCLASS that
 *                                      inherits the getters being replaced
 *   allocateDirect                   — `DirectByteBuffer`, a different class
 *                                      that must be left alone
 *   BIG_ENDIAN / LITTLE_ENDIAN       — the `bigEndian` field selects assembly
 *   limit() below capacity           — bounds are checked against LIMIT, and
 *                                      getting that wrong reads live bytes
 *                                      that Java says are out of range
 *   out-of-range indices             — exception CLASS and MESSAGE, per width
 *
 * Every line prints a value. Nothing here asserts; the diff is the test.
 */
public final class ByteBufferAccessorMatrixProbe {

	public static void main(String[] args) {
		for (ByteOrder order : new ByteOrder[] { ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN }) {
			shape("allocate", fill(ByteBuffer.allocate(64)).order(order));
			shape("wrap", fill(ByteBuffer.wrap(bytes(64))).order(order));
			shape("wrap(off,len)", fill(ByteBuffer.wrap(bytes(64), 8, 40)).order(order));
			shape("slice@8", slice(fill(ByteBuffer.allocate(64)), 8).order(order));
			shape("duplicate", fill(ByteBuffer.allocate(64)).duplicate().order(order));
			shape("readOnly", fill(ByteBuffer.allocate(64)).asReadOnlyBuffer().order(order));
			shape("direct", fill(ByteBuffer.allocateDirect(64)).order(order));
		}
		bounds();
		relative();
		limitBelowCapacity();
		floats();
	}

	private static void shape(String name, ByteBuffer buffer) {
		String tag = name + "/" + (buffer.order() == ByteOrder.BIG_ENDIAN ? "BE" : "LE");
		System.out.println(tag + " class=" + buffer.getClass().getSimpleName() + " cap=" + buffer.capacity()
				+ " pos=" + buffer.position() + " limit=" + buffer.limit() + " ro=" + buffer.isReadOnly()
				+ " direct=" + buffer.isDirect());
		System.out.println(tag + " get(0)=" + buffer.get(0) + " get(3)=" + buffer.get(3));
		System.out.println(tag + " getShort(0)=" + buffer.getShort(0) + " getShort(1)=" + buffer.getShort(1));
		System.out.println(tag + " getChar(0)=" + (int) buffer.getChar(0) + " getChar(5)=" + (int) buffer.getChar(5));
		System.out.println(tag + " getInt(0)=" + buffer.getInt(0) + " getInt(3)=" + buffer.getInt(3));
		System.out.println(tag + " getLong(0)=" + buffer.getLong(0) + " getLong(7)=" + buffer.getLong(7));
	}

	/** Exception class and message for every out-of-range shape, per width. */
	private static void bounds() {
		ByteBuffer buffer = fill(ByteBuffer.allocate(16));
		System.out.println("-- bounds, capacity=16 limit=16");
		oob("get(-1)", () -> buffer.get(-1));
		oob("get(16)", () -> buffer.get(16));
		oob("getShort(15)", () -> buffer.getShort(15));
		oob("getShort(-1)", () -> buffer.getShort(-1));
		oob("getInt(13)", () -> buffer.getInt(13));
		oob("getInt(16)", () -> buffer.getInt(16));
		oob("getLong(9)", () -> buffer.getLong(9));
		oob("getChar(15)", () -> buffer.getChar(15));
		oob("getInt(MAX)", () -> buffer.getInt(Integer.MAX_VALUE));
		oob("getInt(MIN)", () -> buffer.getInt(Integer.MIN_VALUE));
		// The last legal index of each width must NOT throw.
		System.out.println("legal getShort(14)=" + buffer.getShort(14));
		System.out.println("legal getInt(12)=" + buffer.getInt(12));
		System.out.println("legal getLong(8)=" + buffer.getLong(8));
	}

	/** Relative reads advance `position`; the amount is the width. */
	private static void relative() {
		ByteBuffer buffer = fill(ByteBuffer.allocate(32));
		System.out.println("-- relative");
		System.out.println("get()=" + buffer.get() + " pos=" + buffer.position());
		System.out.println("getShort()=" + buffer.getShort() + " pos=" + buffer.position());
		System.out.println("getChar()=" + (int) buffer.getChar() + " pos=" + buffer.position());
		System.out.println("getInt()=" + buffer.getInt() + " pos=" + buffer.position());
		System.out.println("getLong()=" + buffer.getLong() + " pos=" + buffer.position());
		buffer.position(30);
		oob("relative getInt at pos=30", buffer::getInt);
		System.out.println("pos after failed relative read=" + buffer.position());
	}

	/** Bounds are the LIMIT, not the capacity — bytes past it are unreadable. */
	private static void limitBelowCapacity() {
		ByteBuffer buffer = fill(ByteBuffer.allocate(32));
		buffer.limit(10);
		System.out.println("-- limit=10 of capacity=32");
		System.out.println("legal getShort(8)=" + buffer.getShort(8));
		oob("getShort(9) past limit", () -> buffer.getShort(9));
		oob("getInt(8) past limit", () -> buffer.getInt(8));
		oob("get(10) past limit", () -> buffer.get(10));
	}

	/** Float/double go through the same path and must keep their bit pattern. */
	private static void floats() {
		ByteBuffer buffer = ByteBuffer.allocate(32);
		buffer.putFloat(0, Float.NaN);
		buffer.putFloat(4, -0.0f);
		buffer.putFloat(8, Float.MIN_VALUE);
		buffer.putDouble(16, Double.NaN);
		buffer.putDouble(24, -0.0d);
		System.out.println("-- float/double");
		System.out.println("getFloat(0) bits=" + Float.floatToRawIntBits(buffer.getFloat(0)));
		System.out.println("getFloat(4) bits=" + Float.floatToRawIntBits(buffer.getFloat(4)));
		System.out.println("getFloat(8) bits=" + Float.floatToRawIntBits(buffer.getFloat(8)));
		System.out.println("getDouble(16) bits=" + Double.doubleToRawLongBits(buffer.getDouble(16)));
		System.out.println("getDouble(24) bits=" + Double.doubleToRawLongBits(buffer.getDouble(24)));
		// A round trip through the LE view must come back identical.
		ByteBuffer le = ByteBuffer.allocate(16).order(ByteOrder.LITTLE_ENDIAN);
		le.putInt(0, 0x01020304);
		le.putLong(8, 0x0102030405060708L);
		System.out.println("LE putInt/getInt=" + Integer.toHexString(le.getInt(0)) + " byte0="
				+ le.get(0) + " byte3=" + le.get(3));
		System.out.println("LE putLong/getLong=" + Long.toHexString(le.getLong(8)) + " byte8=" + le.get(8));
	}

	private static void oob(String what, ThrowingRead read) {
		try {
			read.run();
			System.out.println(what + " -> NO EXCEPTION");
		}
		catch (Throwable ex) {
			System.out.println(what + " -> " + ex.getClass().getName() + ": " + ex.getMessage());
		}
	}

	/**
	 * Writes 1..limit. Bounded by `limit`, not `capacity`, so a
	 * `wrap(array, off, len)` view — whose limit is below its capacity — is
	 * filled through its own window rather than throwing.
	 */
	private static ByteBuffer fill(ByteBuffer buffer) {
		if (buffer.isReadOnly()) {
			return buffer;
		}
		for (int i = 0; i < buffer.limit(); i++) {
			buffer.put(i, (byte) (i + 1));
		}
		return buffer;
	}

	private static ByteBuffer slice(ByteBuffer buffer, int at) {
		buffer.position(at);
		ByteBuffer sliced = buffer.slice();
		((Buffer) buffer).position(0);
		return sliced;
	}

	private static byte[] bytes(int n) {
		byte[] out = new byte[n];
		for (int i = 0; i < n; i++) {
			out[i] = (byte) (i + 1);
		}
		return out;
	}

	@FunctionalInterface
	private interface ThrowingRead {

		void run() throws Throwable;

	}

}
