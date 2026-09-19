import java.io.*;
import java.nio.*;
import java.nio.channels.*;
import java.nio.charset.*;
import java.nio.file.*;
import java.util.*;

/**
 * The NIO half of this lane's brief — `ByteBuffer` and `FileChannel`.
 *
 * `WORKER-4-1` through `-4-3` probed the `java.io` census classes and the NIO
 * RECEIVERS (`W4Abstract`, 63 of them). What none of them asked is whether the
 * buffer and channel BODIES answer correctly: `native-io/src/direct_buffer.rs`,
 * `nio_native.rs` and the `FileChannel` family are large surfaces whose only
 * gate is `RJdkNio`, and a corpus vector asks the questions its author thought
 * of.
 *
 * Everything here has an answer fixed by the javadoc. Buffer state is printed
 * as `pos/lim/cap` on every step, because the three move together and a body
 * that updates two of them is the failure this shape has.
 */
public class W4Nio {

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
        return b.position() + "/" + b.limit() + "/" + b.capacity()
                + (b.hasRemaining() ? "" : " EMPTY");
    }

    static String hex(ByteBuffer b) {
        ByteBuffer d = b.duplicate();
        StringBuilder sb = new StringBuilder();
        while (d.hasRemaining()) sb.append(String.format("%02x", d.get()));
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        // ---- allocation and initial state --------------------------------
        ByteBuffer heap = ByteBuffer.allocate(16);
        ck("alloc.state", st(heap));
        ck("alloc.isDirect", heap.isDirect());
        ck("alloc.hasArray", heap.hasArray());
        ck("alloc.arrayOffset", heap.arrayOffset());
        ck("alloc.order", heap.order());
        ck("alloc.isReadOnly", heap.isReadOnly());

        ByteBuffer direct = ByteBuffer.allocateDirect(16);
        ck("direct.state", st(direct));
        ck("direct.isDirect", direct.isDirect());
        ck("direct.hasArray", direct.hasArray());
        ck("direct.order", direct.order());
        ckT("direct.array", () -> direct.array());

        byte[] backing = {1, 2, 3, 4, 5, 6, 7, 8};
        ByteBuffer wrapped = ByteBuffer.wrap(backing);
        ck("wrap.state", st(wrapped));
        ck("wrap.hasArray", wrapped.hasArray());
        ck("wrap.sameArray", wrapped.array() == backing);
        ByteBuffer wrapRange = ByteBuffer.wrap(backing, 2, 4);
        ck("wrapRange.state", st(wrapRange));
        ck("wrapRange.arrayOffset", wrapRange.arrayOffset());

        // ---- put / get and the cursor ------------------------------------
        ByteBuffer b = ByteBuffer.allocate(16);
        b.put((byte) 0x11);
        ck("put.byte.state", st(b));
        b.putShort((short) 0x2233);
        ck("put.short.state", st(b));
        b.putInt(0x44556677);
        ck("put.int.state", st(b));
        b.putLong(0x8899aabbccddeeffL);
        ck("put.long.state", st(b));
        ck("put.hexAfterFlip", (Object) (new Object() {
            String go() { b.flip(); return hex(b) + " " + st(b); }
        }).go());
        b.rewind();
        ck("get.byte", String.format("%02x", b.get()));
        ck("get.short", String.format("%04x", b.getShort()));
        ck("get.int", String.format("%08x", b.getInt()));
        ck("get.long", String.format("%016x", b.getLong()));
        ck("get.state", st(b));
        ckT("get.past", () -> b.get());

        // ---- absolute accessors must NOT move the cursor -------------------
        ByteBuffer abs = ByteBuffer.allocate(8);
        abs.putInt(0, 0x01020304);
        ck("abs.put.state", st(abs));
        ck("abs.get", String.format("%08x", abs.getInt(0)));
        ck("abs.get.state", st(abs));
        ckT("abs.oob", () -> abs.getInt(6));
        ckT("abs.negative", () -> abs.getInt(-1));

        // ---- byte order ----------------------------------------------------
        ByteBuffer le = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN);
        le.putInt(0x01020304).flip();
        ck("order.le.bytes", hex(le));
        ck("order.le.readBack", String.format("%08x", le.getInt(0)));
        ByteBuffer be = ByteBuffer.allocate(8).order(ByteOrder.BIG_ENDIAN);
        be.putInt(0x01020304).flip();
        ck("order.be.bytes", hex(be));
        ck("order.nativeIsOneOf",
                ByteOrder.nativeOrder() == ByteOrder.LITTLE_ENDIAN
                        || ByteOrder.nativeOrder() == ByteOrder.BIG_ENDIAN);
        // A slice inherits BIG_ENDIAN, not the parent's order — a JDK quirk
        // that a hand-written slice usually gets wrong.
        ByteBuffer leSlice = ByteBuffer.allocate(8).order(ByteOrder.LITTLE_ENDIAN).slice();
        ck("order.sliceResets", leSlice.order());

        // ---- slice / duplicate share the STORE, not the cursor ---------------
        ByteBuffer parent = ByteBuffer.wrap(new byte[] {10, 20, 30, 40, 50, 60});
        parent.position(2).limit(5);
        ByteBuffer sl = parent.slice();
        ck("slice.state", st(sl));
        ck("slice.contents", hex(sl));
        sl.put(0, (byte) 99);
        ck("slice.writeVisibleInParent", parent.get(2));
        ck("slice.parentUnmoved", st(parent));
        ByteBuffer dup = parent.duplicate();
        ck("dup.state", st(dup));
        dup.position(4);
        ck("dup.independentCursor", st(parent) + " vs " + st(dup));

        // ---- compact / mark / reset / clear -----------------------------------
        ByteBuffer c = ByteBuffer.wrap(new byte[] {1, 2, 3, 4, 5, 6});
        c.position(2);
        c.compact();
        ck("compact.state", st(c));
        ck("compact.contents", (Object) (new Object() {
            String go() { ByteBuffer d = c.duplicate(); d.flip(); return hex(d); }
        }).go());
        ByteBuffer m = ByteBuffer.wrap(new byte[] {1, 2, 3, 4});
        m.position(1).mark();
        m.position(3);
        m.reset();
        ck("mark.reset.state", st(m));
        m.clear();
        ck("clear.state", st(m));
        ckT("reset.noMark", () -> { ByteBuffer x = ByteBuffer.allocate(4); x.reset(); return "no-throw"; });

        // ---- read-only views ---------------------------------------------------
        ByteBuffer ro = ByteBuffer.wrap(new byte[] {7, 8, 9}).asReadOnlyBuffer();
        ck("ro.isReadOnly", ro.isReadOnly());
        ck("ro.get", ro.get(0));
        ckT("ro.put", () -> { ro.put(0, (byte) 1); return "no-throw"; });
        ckT("ro.array", () -> ro.array());

        // ---- typed views --------------------------------------------------------
        ByteBuffer viewSrc = ByteBuffer.allocate(16).order(ByteOrder.BIG_ENDIAN);
        IntBuffer ints = viewSrc.asIntBuffer();
        ck("asIntBuffer.state", st(ints));
        ints.put(0, 0x01020304);
        ck("asIntBuffer.writeVisible", String.format("%08x", viewSrc.getInt(0)));
        CharBuffer chars = ByteBuffer.allocate(8).asCharBuffer();
        ck("asCharBuffer.state", st(chars));
        ck("charBuffer.wrapString", CharBuffer.wrap("hello").toString());

        // ---- charset decode/encode through buffers -------------------------------
        ByteBuffer enc = StandardCharsets.UTF_8.encode("café ✓");
        ck("charset.encode.hex", hex(enc));
        ck("charset.decode", StandardCharsets.UTF_8.decode(enc).toString());
        CharsetDecoder dec = StandardCharsets.UTF_8.newDecoder();
        ck("decoder.malformedAction", dec.malformedInputAction());
        ckT("decoder.strictOnBadInput", () -> dec
                .onMalformedInput(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(new byte[] {(byte) 0xC3})));

        // ---- FileChannel ----------------------------------------------------------
        Path dir = Files.createTempDirectory("w4nio");
        Path f = dir.resolve("data.bin");
        Files.write(f, "0123456789".getBytes(StandardCharsets.UTF_8));

        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ)) {
            ck("fc.size", ch.size());
            ck("fc.position0", ch.position());
            ByteBuffer dst = ByteBuffer.allocate(4);
            ck("fc.read.n", ch.read(dst));
            ck("fc.read.dstState", st(dst));
            ck("fc.read.contents", new String(dst.array(), 0, dst.position(), StandardCharsets.UTF_8));
            ck("fc.positionAfterRead", ch.position());
            dst.clear();
            ck("fc.readAbsolute.n", ch.read(dst, 8));
            ck("fc.readAbsolute.positionUnmoved", ch.position());
            ck("fc.readAbsolute.contents",
                    new String(dst.array(), 0, dst.position(), StandardCharsets.UTF_8));
            ch.position(2);
            ck("fc.positionSet", ch.position());
            ck("fc.isOpen", ch.isOpen());
        }

        Path w = dir.resolve("written.bin");
        try (FileChannel ch = FileChannel.open(w,
                StandardOpenOption.CREATE, StandardOpenOption.WRITE, StandardOpenOption.READ)) {
            ck("fc.write.n", ch.write(ByteBuffer.wrap("abcdef".getBytes(StandardCharsets.UTF_8))));
            ck("fc.write.size", ch.size());
            ch.truncate(3);
            ck("fc.truncate.size", ch.size());
            ck("fc.truncate.position", ch.position());
            ch.force(true);
            ck("fc.force", "ok");
            try (FileChannel src = FileChannel.open(f, StandardOpenOption.READ)) {
                ck("fc.transferFrom.n", ch.transferFrom(src, 3, 10));
            }
            ck("fc.afterTransfer.size", ch.size());
        }
        ck("fc.fileContents", new String(Files.readAllBytes(w), StandardCharsets.UTF_8));

        try (FileChannel ch = FileChannel.open(w, StandardOpenOption.READ);
             ByteArrayOutputStream sink = new ByteArrayOutputStream();
             WritableByteChannel out = Channels.newChannel(sink)) {
            ck("fc.transferTo.n", ch.transferTo(0, 100, out));
            ck("fc.transferTo.contents", sink.toString("UTF-8"));
        }

        // A closed channel refuses, and the type is fixed.
        FileChannel closed = FileChannel.open(f, StandardOpenOption.READ);
        closed.close();
        ck("fc.closed.isOpen", closed.isOpen());
        ckT("fc.closed.read", () -> closed.read(ByteBuffer.allocate(4)));
        ckT("fc.closed.size", () -> closed.size());
        ckT("fc.closed.position", () -> closed.position());

        // Reading into a read-only buffer is a fixed refusal.
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ)) {
            ckT("fc.readIntoReadOnly",
                    () -> ch.read(ByteBuffer.allocate(4).asReadOnlyBuffer()));
        }
        // Writing to a read-only channel likewise.
        try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ)) {
            ckT("fc.writeToReadOnly",
                    () -> ch.write(ByteBuffer.wrap(new byte[] {1})));
        }

        System.out.println("PASS W4Nio");
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
