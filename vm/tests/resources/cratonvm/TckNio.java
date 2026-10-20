package cratonvm;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.IntBuffer;

/**
 * JCK-style conformance tests for java.nio (NEW-16.2).
 *
 * All tests are static, take no arguments, and return 1 on pass / 0 on fail.
 * No I/O is performed — only in-memory buffer semantics per JSR-51.
 */
public class TckNio {

    // ByteBuffer.allocate creates a zero-initialized heap buffer
    public static int bb_allocate() {
        ByteBuffer bb = ByteBuffer.allocate(16);
        if (bb == null) return 0;
        if (bb.capacity() != 16) return 0;
        if (bb.position() != 0) return 0;
        if (bb.limit() != 16) return 0;
        return 1;
    }

    // ByteBuffer.wrap uses the backing array directly
    public static int bb_wrap() {
        byte[] src = new byte[] { 1, 2, 3, 4 };
        ByteBuffer bb = ByteBuffer.wrap(src);
        if (bb.capacity() != 4) return 0;
        if (bb.get(0) != 1) return 0;
        if (bb.get(3) != 4) return 0;
        return 1;
    }

    // Sequential put/get advance position
    public static int bb_put_get_sequential() {
        ByteBuffer bb = ByteBuffer.allocate(4);
        bb.put((byte) 10).put((byte) 20).put((byte) 30).put((byte) 40);
        if (bb.position() != 4) return 0;
        bb.flip();
        if (bb.get() != 10) return 0;
        if (bb.get() != 20) return 0;
        if (bb.get() != 30) return 0;
        if (bb.get() != 40) return 0;
        return 1;
    }

    // flip: limit = position, position = 0
    public static int bb_flip() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 1).put((byte) 2).put((byte) 3);
        bb.flip();
        if (bb.position() != 0) return 0;
        if (bb.limit() != 3) return 0;
        return 1;
    }

    // clear: position = 0, limit = capacity
    public static int bb_clear() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 1).put((byte) 2);
        bb.clear();
        if (bb.position() != 0) return 0;
        if (bb.limit() != 8) return 0;
        return 1;
    }

    // putInt writes 4 bytes in current byte order
    public static int bb_put_int() {
        ByteBuffer bb = ByteBuffer.allocate(4).order(ByteOrder.BIG_ENDIAN);
        bb.putInt(0x01020304);
        bb.flip();
        if (bb.get() != 0x01) return 0;
        if (bb.get() != 0x02) return 0;
        if (bb.get() != 0x03) return 0;
        if (bb.get() != 0x04) return 0;
        return 1;
    }

    // Little-endian ordering
    public static int bb_little_endian() {
        ByteBuffer bb = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN);
        bb.putInt(0x01020304);
        bb.flip();
        if (bb.get() != 0x04) return 0;
        if (bb.get() != 0x03) return 0;
        if (bb.get() != 0x02) return 0;
        if (bb.get() != 0x01) return 0;
        return 1;
    }

    // remaining() = limit - position
    public static int bb_remaining() {
        ByteBuffer bb = ByteBuffer.allocate(10);
        if (bb.remaining() != 10) return 0;
        bb.put((byte) 1).put((byte) 2).put((byte) 3);
        if (bb.remaining() != 7) return 0;
        return 1;
    }

    // IntBuffer wrapping
    public static int ib_wrap() {
        int[] src = new int[] { 100, 200, 300 };
        IntBuffer ib = IntBuffer.wrap(src);
        if (ib.capacity() != 3) return 0;
        if (ib.get(0) != 100) return 0;
        if (ib.get(1) != 200) return 0;
        if (ib.get(2) != 300) return 0;
        return 1;
    }

    // duplicate shares content but has independent position/limit
    public static int bb_duplicate() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 5).put((byte) 10).put((byte) 15).put((byte) 20);
        bb.flip();
        ByteBuffer dup = bb.duplicate();
        // dup has same position, limit, capacity
        if (dup.position() != 0) return 0;
        if (dup.limit() != 4) return 0;
        // read from dup does not advance original
        if (dup.get() != 5) return 0;
        if (dup.get() != 10) return 0;
        if (bb.position() != 0) return 0; // original unchanged
        // write through dup is visible in original
        dup.put(2, (byte) 99);
        if (bb.get(2) != 99) return 0;
        return 1;
    }

    // slice creates a view of remaining elements
    public static int bb_slice() {
        ByteBuffer bb = ByteBuffer.allocate(8);
        bb.put((byte) 1).put((byte) 2).put((byte) 3).put((byte) 4);
        bb.flip();
        bb.get(); // consume 1
        bb.get(); // consume 2
        // position=2, limit=4, so slice sees [3, 4]
        ByteBuffer sl = bb.slice();
        if (sl.capacity() != 2) return 0;
        if (sl.position() != 0) return 0;
        if (sl.limit() != 2) return 0;
        if (sl.get() != 3) return 0;
        if (sl.get() != 4) return 0;
        return 1;
    }

    // array() returns the backing heap array
    public static int bb_array() {
        ByteBuffer bb = ByteBuffer.allocate(4);
        bb.put((byte) 11).put((byte) 22).put((byte) 33).put((byte) 44);
        if (!bb.hasArray()) return 0;
        byte[] arr = bb.array();
        if (arr.length != 4) return 0;
        if (arr[0] != 11) return 0;
        if (arr[1] != 22) return 0;
        if (arr[2] != 33) return 0;
        if (arr[3] != 44) return 0;
        // modifications to array visible through buffer
        arr[0] = 99;
        if (bb.get(0) != 99) return 0;
        return 1;
    }

    // hasRemaining false at end
    public static int bb_has_remaining() {
        ByteBuffer bb = ByteBuffer.allocate(2);
        if (!bb.hasRemaining()) return 0;
        bb.put((byte) 1).put((byte) 2);
        if (bb.hasRemaining()) return 0;
        return 1;
    }
}
