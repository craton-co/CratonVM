import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.IntBuffer;
import java.nio.ShortBuffer;
import java.util.Arrays;

/**
 * JDK-only corpus: {@code ByteBuffer.order(ByteOrder)} and everything that
 * reads the flag it sets.
 *
 * WHY THIS VECTOR EXISTS, and why every assertion below is about CONTENTS.
 *
 * Under {@code --synthetic-jdk}, {@code order(ByteOrder)} used to write the
 * byte-order flag over slot 0 of the receiver -- the BACKING ARRAY. The
 * layout screen it consulted asked {@code object_num_fields(buf) == 6}, and a
 * fabricated {@code java.nio.ByteBuffer} is ten fields wide, so a genuine
 * synthetic buffer was classified as a real JDK layout, had no {@code
 * bigEndian} field to write by name, and fell through to the indexed slot the
 * array lives in. {@code ByteBuffer.allocate(8).order(ByteOrder.nativeOrder())}
 * took the VM down:
 *
 * <pre>
 *   internal error: ByteBuffer missing backing storage
 *     (hb/slot5/address absent; field 0 returned Int(1), address Object(None))
 * </pre>
 *
 * exit 1, no Java exception.
 *
 * THE CORRUPTION WAS SELF-CONSISTENT. The reader read the same slot back, so
 * {@code order()} still ANSWERED {@code LITTLE_ENDIAN} over the destroyed
 * buffer -- a vector that only asked {@code order()} would have read green
 * while the object was already unusable. So: this file never treats a
 * {@code order()} answer as the evidence. It puts known bytes in, changes the
 * order, and asserts the BYTES and the multi-byte accessors afterwards, plus
 * the IDENTITY of the backing array across the transition. Non-null is not the
 * contract; two defects survived this year behind {@code != null} and check
 * counts.
 *
 * ORACLE NOTES, measured against HotSpot 25 rather than assumed:
 *  - {@code order(bo)} returns the receiver itself (identity, not a copy).
 *  - {@code slice()} / {@code duplicate()} / {@code asReadOnlyBuffer()} do NOT
 *    carry the source's order: {@code boolean bigEndian = true} is a FIELD
 *    INITIALISER on {@code ByteBuffer}, so every derived buffer is born
 *    BIG_ENDIAN however the source was set. They preserve CONTENT, not ORDER.
 *  - {@code as<T>Buffer()} DOES carry it: the JDK picks a concrete
 *    {@code ByteBufferAs<T>Buffer{B,L}} class from the source's order at
 *    construction time.
 * Both directions are asserted, because a VM that propagated the order
 * everywhere and a VM that propagated it nowhere each pass one half.
 */
public class RJdkByteOrder {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    /** The eight bytes every case below starts from: 01 02 03 04 05 06 07 08. */
    static final byte[] SEED = { 1, 2, 3, 4, 5, 6, 7, 8 };

    static byte[] bytesOf(ByteBuffer b) {
        byte[] out = new byte[b.capacity()];
        for (int i = 0; i < out.length; i++) {
            out[i] = b.get(i);
        }
        return out;
    }

    static String hex(byte[] a) {
        StringBuilder sb = new StringBuilder();
        for (byte v : a) {
            sb.append(String.format("%02x", v));
        }
        return sb.toString();
    }

    static void seed(ByteBuffer b) {
        for (int i = 0; i < SEED.length; i++) {
            b.put(i, SEED[i]);
        }
    }

    /**
     * ByteOrder is two singletons plus a derived third. Identity, not equality:
     * JDK and library bytecode compares with {@code ==} (Lucene asserts
     * {@code buffer.order() == LITTLE_ENDIAN} outright), so a fresh equal-valued
     * instance is a wrong answer that {@code equals} would not catch.
     */
    static void constants() {
        ByteOrder be = ByteOrder.BIG_ENDIAN;
        ByteOrder le = ByteOrder.LITTLE_ENDIAN;
        check(be == ByteOrder.BIG_ENDIAN, "BIG_ENDIAN must be a stable singleton");
        check(le == ByteOrder.LITTLE_ENDIAN, "LITTLE_ENDIAN must be a stable singleton");
        check(be != le, "the two constants must be distinct objects");
        check(!be.equals(le), "and must not compare equal");
        check(be.toString().equals("BIG_ENDIAN"), "BIG_ENDIAN.toString(): " + be);
        check(le.toString().equals("LITTLE_ENDIAN"), "LITTLE_ENDIAN.toString(): " + le);

        ByteOrder nat = ByteOrder.nativeOrder();
        check(nat == be || nat == le,
                "nativeOrder() must BE one of the two constants, not an equal-valued copy: " + nat);
        check(nat == ByteOrder.nativeOrder(), "nativeOrder() must be stable across calls");
        // Pin the platform without hardcoding it: the VM under test and the
        // HotSpot oracle run on the same machine, so this line is comparable.
        System.out.println("CK RJdkByteOrder native=" + nat);
    }

    /**
     * THE DEFECT, on a heap buffer. Bytes in, order changed, bytes out.
     */
    static void heapOrderChangePreservesStorage() {
        ByteBuffer b = ByteBuffer.allocate(8);
        seed(b);
        byte[] before = bytesOf(b);
        check(Arrays.equals(before, SEED), "seeded bytes: " + hex(before));
        check(b.hasArray(), "a heap buffer has an accessible array");
        byte[] backing = b.array();
        check(b.order() == ByteOrder.BIG_ENDIAN, "a fresh buffer is BIG_ENDIAN");
        check(b.getInt(0) == 0x01020304, "BE getInt");
        check(b.getShort(0) == (short) 0x0102, "BE getShort");
        check(b.getLong(0) == 0x0102030405060708L, "BE getLong");

        ByteBuffer same = b.order(ByteOrder.LITTLE_ENDIAN);
        check(same == b, "order(ByteOrder) returns the receiver itself");

        // The storage assertions. These are the ones that fail on the defect;
        // the order assertion below passes even while the buffer is destroyed.
        check(b.array() == backing,
                "order() must not replace the backing array -- identity, not non-null");
        check(Arrays.equals(bytesOf(b), SEED),
                "order() must not disturb a single byte: " + hex(bytesOf(b)));
        check(b.capacity() == 8 && b.limit() == 8 && b.position() == 0,
                "order() must not disturb position/limit/capacity");

        check(b.order() == ByteOrder.LITTLE_ENDIAN, "the order did take effect");
        check(b.getInt(0) == 0x04030201, "LE getInt: " + Integer.toHexString(b.getInt(0)));
        check(b.getShort(0) == (short) 0x0201, "LE getShort");
        check(b.getLong(0) == 0x0807060504030201L, "LE getLong");

        // And back again. A writer that only ever SET the flag passes the
        // forward half on its own.
        b.order(ByteOrder.BIG_ENDIAN);
        check(b.order() == ByteOrder.BIG_ENDIAN, "the order is reversible");
        check(b.array() == backing, "the backing array survives the second transition too");
        check(Arrays.equals(bytesOf(b), SEED), "and so do the bytes");
        check(b.getInt(0) == 0x01020304, "BE getInt after the round trip");
        check(b.getLong(0) == 0x0102030405060708L, "BE getLong after the round trip");

        // nativeOrder() is how the defect was originally reached (the
        // LITTLE_ENDIAN constant is unavailable in some configurations while
        // nativeOrder() is), so drive it explicitly rather than only via the
        // constants.
        b.order(ByteOrder.nativeOrder());
        check(b.order() == ByteOrder.nativeOrder(), "order(nativeOrder()) round-trips");
        check(b.array() == backing, "and does not touch the storage");
        check(Arrays.equals(bytesOf(b), SEED), "nor the bytes");

        System.out.println("CK RJdkByteOrder heap bytes=" + hex(bytesOf(b)) + " order=" + b.order());
    }

    /** Relative puts must lay bytes down in the CURRENT order. */
    static void relativeAccessorsRespectOrder() {
        ByteBuffer le = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
        le.putInt(0x01020304);
        le.putShort((short) 0x0506);
        le.putShort((short) 0x0708);
        check(le.position() == 8, "position after relative puts: " + le.position());
        check(hex(bytesOf(le)).equals("0403020106050807"),
                "little-endian layout on disk: " + hex(bytesOf(le)));

        ByteBuffer be = ByteBuffer.allocate(8).order(ByteOrder.BIG_ENDIAN);
        be.putInt(0x01020304);
        be.putShort((short) 0x0506);
        be.putShort((short) 0x0708);
        check(hex(bytesOf(be)).equals("0102030405060708"),
                "big-endian layout on disk: " + hex(bytesOf(be)));

        // Same bytes, opposite readers.
        ByteBuffer r = ByteBuffer.wrap(SEED.clone());
        check(r.getInt(0) == 0x01020304, "wrap() is BIG_ENDIAN by default");
        byte[] wrapped = r.array();
        r.order(ByteOrder.LITTLE_ENDIAN);
        check(r.array() == wrapped, "order() on a wrapped buffer keeps the caller's array");
        check(Arrays.equals(r.array(), SEED), "and does not rewrite the caller's array");
        check(r.getInt(0) == 0x04030201, "LE read over the wrapped array");
        System.out.println("CK RJdkByteOrder relative le=" + hex(bytesOf(le))
                + " be=" + hex(bytesOf(be)));
    }

    /** The same transition on a DIRECT buffer, which has no array at all. */
    static void directOrderChangePreservesStorage() {
        ByteBuffer d = ByteBuffer.allocateDirect(8);
        check(d.isDirect(), "allocateDirect must be direct");
        check(!d.hasArray(), "a direct buffer exposes no array");
        seed(d);
        check(Arrays.equals(bytesOf(d), SEED), "seeded direct bytes: " + hex(bytesOf(d)));
        check(d.order() == ByteOrder.BIG_ENDIAN, "a fresh direct buffer is BIG_ENDIAN");
        check(d.getLong(0) == 0x0102030405060708L, "BE direct getLong");

        ByteBuffer same = d.order(ByteOrder.LITTLE_ENDIAN);
        check(same == d, "order(ByteOrder) returns the receiver itself for a direct buffer too");
        check(d.isDirect(), "it is still direct");
        check(Arrays.equals(bytesOf(d), SEED),
                "the off-heap block must be untouched: " + hex(bytesOf(d)));
        check(d.getInt(0) == 0x04030201, "LE direct getInt");
        check(d.getLong(0) == 0x0807060504030201L, "LE direct getLong");

        d.order(ByteOrder.BIG_ENDIAN);
        check(Arrays.equals(bytesOf(d), SEED), "and after the reverse transition");
        check(d.getInt(0) == 0x01020304, "BE direct getInt after the round trip");
        System.out.println("CK RJdkByteOrder direct bytes=" + hex(bytesOf(d)));
    }

    /**
     * Typed views DO inherit the source's order -- the JDK picks a concrete
     * {@code ByteBufferAs<T>Buffer{B,L}} class at construction. Asserted in both
     * directions so a VM that hardcodes either endianness fails.
     */
    static void typedViewsCarryTheOrder() {
        ByteBuffer src = ByteBuffer.allocate(8);
        seed(src);

        IntBuffer ibBe = src.order(ByteOrder.BIG_ENDIAN).asIntBuffer();
        check(ibBe.capacity() == 2, "asIntBuffer capacity: " + ibBe.capacity());
        check(ibBe.order() == ByteOrder.BIG_ENDIAN, "a view of a BE buffer is BE");
        check(ibBe.get(0) == 0x01020304, "BE int view [0]: " + Integer.toHexString(ibBe.get(0)));
        check(ibBe.get(1) == 0x05060708, "BE int view [1]");

        IntBuffer ibLe = src.order(ByteOrder.LITTLE_ENDIAN).asIntBuffer();
        check(ibLe.order() == ByteOrder.LITTLE_ENDIAN, "a view of an LE buffer is LE");
        check(ibLe.get(0) == 0x04030201, "LE int view [0]: " + Integer.toHexString(ibLe.get(0)));
        check(ibLe.get(1) == 0x08070605, "LE int view [1]");

        ShortBuffer sbLe = src.asShortBuffer();
        check(sbLe.capacity() == 4, "asShortBuffer capacity: " + sbLe.capacity());
        check(sbLe.order() == ByteOrder.LITTLE_ENDIAN, "short view inherits LE");
        check(sbLe.get(0) == (short) 0x0201, "LE short view [0]");
        check(sbLe.get(3) == (short) 0x0807, "LE short view [3]");

        ShortBuffer sbBe = src.order(ByteOrder.BIG_ENDIAN).asShortBuffer();
        check(sbBe.order() == ByteOrder.BIG_ENDIAN, "short view inherits BE");
        check(sbBe.get(0) == (short) 0x0102, "BE short view [0]");
        check(sbBe.get(3) == (short) 0x0708, "BE short view [3]");

        // The source's bytes are still the seed after all of that.
        check(Arrays.equals(bytesOf(src), SEED),
                "taking views must not disturb the source: " + hex(bytesOf(src)));
        check(src.array() == src.array() && Arrays.equals(src.array(), SEED),
                "and the source's backing array is still the seeded one");
        System.out.println("CK RJdkByteOrder views beInt0=" + Integer.toHexString(ibBe.get(0))
                + " leInt0=" + Integer.toHexString(ibLe.get(0)));
    }

    /**
     * slice()/duplicate()/asReadOnlyBuffer() preserve CONTENT and RESET the
     * order to BIG_ENDIAN. Measured on HotSpot 25, and counter-intuitive enough
     * that a VM "fixing the inconsistency" by propagating the order byteswaps
     * every typed read through such a view.
     */
    static void derivedBuffersResetOrderButKeepContent() {
        ByteBuffer src = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
        seed(src);
        check(src.order() == ByteOrder.LITTLE_ENDIAN, "source is LE");

        ByteBuffer dup = src.duplicate();
        check(dup != src, "duplicate() is a new buffer object");
        check(dup.capacity() == 8, "duplicate capacity");
        check(Arrays.equals(bytesOf(dup), SEED), "duplicate sees the same bytes: " + hex(bytesOf(dup)));
        check(dup.order() == ByteOrder.BIG_ENDIAN,
                "a derived buffer is born BIG_ENDIAN however the source was set: " + dup.order());
        check(dup.getInt(0) == 0x01020304, "so it reads big-endian");
        dup.put(0, (byte) 0x7f);
        check(src.get(0) == 0x7f, "duplicate() ALIASES the source's storage");
        src.put(0, SEED[0]);

        src.position(2);
        ByteBuffer sl = src.slice();
        check(sl.capacity() == 6, "slice capacity: " + sl.capacity());
        check(sl.get(0) == 3, "slice starts at the source's position");
        check(sl.order() == ByteOrder.BIG_ENDIAN, "a slice is BIG_ENDIAN too: " + sl.order());
        check(sl.getInt(0) == 0x03040506, "slice reads big-endian");
        sl.put(0, (byte) 0x7e);
        check(src.get(2) == 0x7e, "slice() ALIASES the source's storage");
        src.put(2, SEED[2]);
        src.position(0);

        ByteBuffer ro = src.asReadOnlyBuffer();
        check(ro.isReadOnly(), "asReadOnlyBuffer is read-only");
        check(ro.order() == ByteOrder.BIG_ENDIAN, "and BIG_ENDIAN: " + ro.order());
        check(Arrays.equals(bytesOf(ro), SEED), "and shows the same bytes");

        // Setting the order on a derived buffer must work on the derived
        // buffer and leave the source alone -- they carry separate flags.
        dup.order(ByteOrder.LITTLE_ENDIAN);
        check(dup.order() == ByteOrder.LITTLE_ENDIAN, "the duplicate's own order changed");
        check(src.order() == ByteOrder.LITTLE_ENDIAN, "the source keeps its own order");
        check(Arrays.equals(bytesOf(src), SEED),
                "and neither buffer's storage moved: " + hex(bytesOf(src)));
        check(Arrays.equals(bytesOf(dup), SEED), "through either handle");
        System.out.println("CK RJdkByteOrder derived dup=" + hex(bytesOf(dup))
                + " sliceCap=" + sl.capacity());
    }

    public static void main(String[] args) {
        constants();
        heapOrderChangePreservesStorage();
        relativeAccessorsRespectOrder();
        directOrderChangePreservesStorage();
        typedViewsCarryTheOrder();
        derivedBuffersResetOrderButKeepContent();
        System.out.println("CK RJdkByteOrder checks=" + checks);
        System.out.println("PASS RJdkByteOrder (" + checks + " checks)");
    }
}
