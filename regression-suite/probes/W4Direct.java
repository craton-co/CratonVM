import java.io.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;

/**
 * `DirectByteBuffer`, put bytes THROUGH it rather than at it.
 *
 * `WORKER-4-NOTE-6` measured 87 NIO cases with zero diffs and then said what it
 * had NOT reached: 68 of 95 buffer/channel §1.4 shadows, of which
 * `java/nio/DirectByteBuffer` is the largest column at 18. That probe allocates
 * a direct buffer and checks its SHAPE — `isDirect`, `hasArray`, the
 * `array()` refusal — and does almost nothing through it. **Direct buffers are
 * where the address arithmetic lives**, and shape questions cannot reach it.
 *
 * Everything here is a value fixed by the javadoc, and every case is run
 * against a HEAP buffer too, so a difference between the two backings shows up
 * as a diff between two lines of this probe rather than needing a second run.
 */
public class W4Direct {

    static void ck(String tag, Object got) { System.out.println("CK " + tag + " " + got); }

    interface Thunk { Object call() throws Exception; }

    static void ckT(String tag, Thunk t) {
        try {
            ck(tag, t.call());
        } catch (Throwable e) {
            ck(tag, "threw:" + e.getClass().getName());
        }
    }

    static String st(Buffer b) {
        return b.position() + "/" + b.limit() + "/" + b.capacity();
    }

    static String hex(ByteBuffer b) {
        ByteBuffer d = b.duplicate();
        StringBuilder sb = new StringBuilder();
        while (d.hasRemaining()) sb.append(String.format("%02x", d.get()));
        return sb.toString();
    }

    /** Every accessor at every width, relative then absolute, on one buffer. */
    static void exercise(String tag, ByteBuffer b) {
        b.clear();
        b.put((byte) 0x7f).put((byte) 0x80);
        b.putChar('Z');
        b.putShort((short) -2);
        b.putInt(Integer.MIN_VALUE);
        b.putLong(Long.MAX_VALUE);
        b.putFloat(-1.5f);
        b.putDouble(Math.PI);
        ck(tag + ".afterPuts", st(b));
        b.flip();
        ck(tag + ".bytes", hex(b));
        b.rewind();
        ck(tag + ".get", b.get());
        ck(tag + ".getUnsigned", b.get() & 0xff);
        ck(tag + ".getChar", b.getChar());
        ck(tag + ".getShort", b.getShort());
        ck(tag + ".getInt", b.getInt());
        ck(tag + ".getLong", b.getLong());
        ck(tag + ".getFloat", b.getFloat());
        ck(tag + ".getDouble", b.getDouble());
        ck(tag + ".exhausted", st(b));

        // Absolute forms must not move the cursor, at every width.
        b.clear();
        b.putInt(4, 0x0a0b0c0d);
        b.putLong(8, -1L);
        b.putChar(16, 'q');
        b.putShort(18, (short) 258);
        b.putFloat(20, 2.5f);
        b.putDouble(24, -0.5d);
        ck(tag + ".abs.cursorUnmoved", st(b));
        ck(tag + ".abs.getInt", String.format("%08x", b.getInt(4)));
        ck(tag + ".abs.getLong", b.getLong(8));
        ck(tag + ".abs.getChar", b.getChar(16));
        ck(tag + ".abs.getShort", b.getShort(18));
        ck(tag + ".abs.getFloat", b.getFloat(20));
        ck(tag + ".abs.getDouble", b.getDouble(24));

        // Bulk transfer both directions.
        byte[] out = new byte[8];
        b.clear();
        b.put("abcdefgh".getBytes(StandardCharsets.UTF_8));
        b.flip();
        b.get(out);
        ck(tag + ".bulkGet", new String(out, StandardCharsets.UTF_8));
        ck(tag + ".bulkGet.state", st(b));
        b.clear();
        b.put("xyz".getBytes(StandardCharsets.UTF_8), 1, 2);
        ck(tag + ".bulkPutRange", st(b));
        b.flip();
        ck(tag + ".bulkPutRange.bytes", hex(b));

        // Refusals.
        ckT(tag + ".getPastLimit", () -> { ByteBuffer d = b.duplicate(); d.position(d.limit()); return d.get(); });
        ckT(tag + ".absOob", () -> b.getInt(b.capacity() - 1));
        ckT(tag + ".absNegative", () -> b.getInt(-1));
        ckT(tag + ".bulkGetTooBig", () -> { ByteBuffer d = b.duplicate(); d.get(new byte[d.capacity() + 1]); return "no-throw"; });
    }

    public static void main(String[] args) throws Exception {
        ByteBuffer direct = ByteBuffer.allocateDirect(32);
        ByteBuffer heap = ByteBuffer.allocate(32);
        ck("direct.isDirect", direct.isDirect());
        ck("heap.isDirect", heap.isDirect());

        exercise("direct", direct);
        exercise("heap", heap);

        // ---- byte order on a DIRECT buffer, which is the case that differs ----
        ByteBuffer dle = ByteBuffer.allocateDirect(8).order(ByteOrder.LITTLE_ENDIAN);
        dle.putInt(0x01020304).flip();
        ck("direct.le.bytes", hex(dle));
        ck("direct.le.readBack", String.format("%08x", dle.getInt(0)));
        ByteBuffer dbe = ByteBuffer.allocateDirect(8).order(ByteOrder.BIG_ENDIAN);
        dbe.putInt(0x01020304).flip();
        ck("direct.be.bytes", hex(dbe));

        // ---- slice / duplicate / read-only OF A DIRECT BUFFER -----------------
        ByteBuffer parent = ByteBuffer.allocateDirect(8);
        parent.put(new byte[] {1, 2, 3, 4, 5, 6, 7, 8});
        parent.position(2).limit(6);
        ByteBuffer sl = parent.slice();
        ck("direct.slice.state", st(sl));
        ck("direct.slice.isDirect", sl.isDirect());
        ck("direct.slice.order", sl.order());
        ck("direct.slice.bytes", hex(sl));
        sl.put(0, (byte) 99);
        ck("direct.slice.writeVisibleInParent", parent.get(2));
        ByteBuffer dup = parent.duplicate();
        ck("direct.dup.isDirect", dup.isDirect());
        ck("direct.dup.state", st(dup));
        ByteBuffer ro = parent.asReadOnlyBuffer();
        ck("direct.ro.isReadOnly", ro.isReadOnly());
        ck("direct.ro.isDirect", ro.isDirect());
        ckT("direct.ro.put", () -> { ro.put(0, (byte) 1); return "no-throw"; });
        ckT("direct.ro.array", () -> ro.array());
        ck("direct.ro.readsThrough", ro.get(2));

        // ---- typed views of a DIRECT buffer ------------------------------------
        ByteBuffer viewSrc = ByteBuffer.allocateDirect(16).order(ByteOrder.BIG_ENDIAN);
        IntBuffer di = viewSrc.asIntBuffer();
        ck("direct.asIntBuffer.state", st(di));
        ck("direct.asIntBuffer.isDirect", di.isDirect());
        di.put(0, 0x01020304);
        ck("direct.asIntBuffer.writeVisible", String.format("%08x", viewSrc.getInt(0)));
        CharBuffer dc = ByteBuffer.allocateDirect(8).asCharBuffer();
        dc.put(0, 'A');
        ck("direct.asCharBuffer.get", dc.get(0));
        LongBuffer dl = ByteBuffer.allocateDirect(16).asLongBuffer();
        dl.put(0, -1L);
        ck("direct.asLongBuffer.get", dl.get(0));

        // ---- a DIRECT buffer through a real FileChannel -------------------------
        // This is the case the address arithmetic must survive: the channel hands
        // the buffer's address to the OS, so a wrong offset shows up as wrong
        // BYTES on disk rather than as an exception.
        Path dir = Files.createTempDirectory("w4direct");
        Path f = dir.resolve("d.bin");
        ByteBuffer wbuf = ByteBuffer.allocateDirect(16);
        wbuf.put("DIRECTWRITE".getBytes(StandardCharsets.UTF_8));
        wbuf.flip();
        try (FileChannel ch = FileChannel.open(f,
                StandardOpenOption.CREATE, StandardOpenOption.WRITE)) {
            ck("direct.fc.write.n", ch.write(wbuf));
        }
        ck("direct.fc.onDisk", new String(Files.readAllBytes(f), StandardCharsets.UTF_8));

        ByteBuffer rbuf = ByteBuffer.allocateDirect(16);
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ)) {
            ck("direct.fc.read.n", ch.read(rbuf));
        }
        rbuf.flip();
        byte[] back = new byte[rbuf.remaining()];
        rbuf.get(back);
        ck("direct.fc.readBack", new String(back, StandardCharsets.UTF_8));

        // A SLICED direct buffer through a channel — the offset case.
        ByteBuffer big = ByteBuffer.allocateDirect(32);
        big.position(8).limit(20);
        ByteBuffer slice = big.slice();
        slice.put("SLICEDBYTES!".getBytes(StandardCharsets.UTF_8));
        slice.flip();
        Path f2 = dir.resolve("s.bin");
        try (FileChannel ch = FileChannel.open(f2,
                StandardOpenOption.CREATE, StandardOpenOption.WRITE)) {
            ck("direct.slice.fc.write.n", ch.write(slice));
        }
        ck("direct.slice.fc.onDisk", new String(Files.readAllBytes(f2), StandardCharsets.UTF_8));

        // Partial read into a buffer with a non-zero position.
        ByteBuffer offset = ByteBuffer.allocateDirect(16);
        offset.position(4);
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ)) {
            ck("direct.fc.readAtOffset.n", ch.read(offset));
        }
        ck("direct.fc.readAtOffset.state", st(offset));
        ck("direct.fc.readAtOffset.byte4", offset.get(4));

        System.out.println("PASS W4Direct");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
