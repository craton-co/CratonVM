// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.BufferOverflowException;
import java.nio.BufferUnderflowException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.ReadOnlyBufferException;

/**
 * Per-element access on a direct ByteBuffer.
 *
 * CratonVM serves DirectByteBuffer.get()/get(int)/put(byte)/put(int,byte) from
 * natives (native-io/src/direct_buffer.rs) instead of running the real-JDK
 * bytecode, because each of those bodies expands to five nested invocations and
 * H2's CompressLZF moves one byte per call — ~65,000 interpreted invokes per
 * 64 KB page, which is the whole of the `nioMemLZF:` throughput residual.
 *
 * A native that replaces a JDK body has to reproduce it exactly, and the parts
 * that are easy to get wrong are all edges rather than the happy path. Each
 * group below is one such edge:
 *
 *   1. sign extension           — a byte >= 0x80 must come back NEGATIVE.
 *   2. slice / duplicate offset — `address` already carries the slice offset;
 *                                 adding `offset` again reads the wrong byte.
 *   3. limit, not capacity      — `bb.limit(n)` must shrink what get(int) will
 *                                 accept, on a buffer whose capacity is larger.
 *   4. relative cursor          — position advances by exactly one, and does
 *                                 NOT advance when the access throws. (The
 *                                 native commits the cursor only after the
 *                                 access succeeds, and the bytecode it bails
 *                                 to runs nextGetIndex() itself; committing
 *                                 first would advance it twice.)
 *   5. read-only                — put must throw and must not write.
 *
 * Heap buffers are covered alongside so a regression that swaps the two
 * dispatch paths is visible.
 *
 * NOTE: unlike most of this suite, groups 1-5 are expected to PASS on the
 * pre-change binary too — the natives are a throughput substitution, not a
 * bug fix, so "identical to the bytecode it replaces" IS the whole contract
 * and both arms must agree. It fails only if the substitution diverges.
 *
 * Groups 6 and 7 are NOT of that kind and are the exception to the paragraph
 * above. They are the scheduled form of probes/DirectByteBufferStateProbe.java,
 * whose HotSpot oracle (probes/DirectByteBufferStateProbe.expected.txt, 282
 * checks measured on Eclipse Adoptium jdk-25.0.3.9) has never had a CratonVM
 * column produced against it, because probes/ is run by nothing —
 * regression-suite/run.sh schedules CORE_CLASSES and JDKONLY_CLASSES only. Each
 * value below is copied from that transcript rather than reasoned about; the row
 * name is quoted in the comment so the two stay traceable to each other.
 *
 * Both groups exercise the s2 ByteBuffer family in native-builtins/src/servlet.rs,
 * which is the registrar that WINS in Compatible mode (nothing overwrites
 * register_s2_bytebuffer there, and array/arrayOffset/hasArray/order/slice/
 * duplicate are all on the forced-native list for java/nio/ByteBuffer, so the
 * native answers even though the real bytecode is loaded). They do NOT reach the
 * native-io family, which is compiled and registered only in a
 * --features synthetic-jdk binary running --synthetic-jdk.
 */
public class RDirectBufferElem {

    static int checks = 0;

    static void check(boolean cond, String what) {
        checks++;
        if (!cond) {
            throw new AssertionError("RDirectBufferElem: " + what);
        }
    }

    static void checkEquals(int expected, int actual, String what) {
        check(expected == actual, what + " (expected " + expected + ", got " + actual + ")");
    }

    /**
     * Run `body`, and check it threw EXACTLY `expected` — not a subclass.
     *
     * {@code checkThrows} below is subclass-tolerant, which is right for the
     * bounds checks but wrong for the array accessors: {@code
     * ReadOnlyBufferException EXTENDS UnsupportedOperationException}, so a
     * tolerant check for the wider type passes against the narrower one and the
     * measured heap/direct split in group 7 would be unobservable in exactly the
     * direction that matters.
     */
    static void checkThrowsExactly(Class<?> expected, Runnable body, String what) {
        checks++;
        try {
            body.run();
        } catch (Throwable t) {
            if (t.getClass() == expected) {
                return;
            }
            throw new AssertionError("RDirectBufferElem: " + what + " threw "
                    + t.getClass().getName() + ", expected exactly " + expected.getName());
        }
        throw new AssertionError("RDirectBufferElem: " + what + " did not throw "
                + expected.getName());
    }

    /** Byte order by NAME — the same string the HotSpot oracle transcript prints. */
    static void checkOrder(String expected, ByteBuffer b, String what) {
        String actual = b.order().toString();
        check(expected.equals(actual), what + " (expected " + expected + ", got " + actual + ")");
    }

    /** Run `body`, and check it threw exactly `expected`. */
    static void checkThrows(Class<?> expected, Runnable body, String what) {
        checks++;
        try {
            body.run();
        } catch (Throwable t) {
            if (expected.isInstance(t)) {
                return;
            }
            throw new AssertionError("RDirectBufferElem: " + what + " threw "
                    + t.getClass().getName() + ", expected " + expected.getName());
        }
        throw new AssertionError("RDirectBufferElem: " + what + " did not throw "
                + expected.getName());
    }

    public static void main(String[] args) {
        absoluteRoundTrip();
        signExtension();
        sliceOffset();
        limitNotCapacity();
        relativeCursor();
        relativeCursorUnchangedOnThrow();
        readOnly();
        bulkAndElementAgree();
        derivedViewOrderIsReset();
        arrayOffsetContract();
        System.out.println("PASS RDirectBufferElem (" + checks + " checks)");
    }

    /** 1. Every index of a direct buffer round-trips independently. */
    static void absoluteRoundTrip() {
        ByteBuffer b = ByteBuffer.allocateDirect(256);
        for (int i = 0; i < 256; i++) {
            b.put(i, (byte) (i ^ 0x5a));
        }
        for (int i = 0; i < 256; i++) {
            checkEquals((byte) (i ^ 0x5a), b.get(i), "absolute round trip at " + i);
        }
    }

    /** 1b. A byte with the top bit set reads back NEGATIVE, not 128..255. */
    static void signExtension() {
        ByteBuffer b = ByteBuffer.allocateDirect(4);
        b.put(0, (byte) 0x80);
        b.put(1, (byte) 0xff);
        b.put(2, (byte) 0x7f);
        b.put(3, (byte) 0x00);
        checkEquals(-128, b.get(0), "get(0) sign extension of 0x80");
        checkEquals(-1, b.get(1), "get(1) sign extension of 0xff");
        checkEquals(127, b.get(2), "get(2) of 0x7f");
        checkEquals(0, b.get(3), "get(3) of 0x00");
        check(b.get(0) < 0, "0x80 must be negative");

        b.position(0);
        checkEquals(-128, b.get(), "relative get sign extension of 0x80");
        checkEquals(-1, b.get(), "relative get sign extension of 0xff");
    }

    /**
     * 2. A slice's element 0 is the parent's element `at`. `Buffer.address`
     * already includes the slice offset, so a native that also adds the
     * `offset` field reads `at` bytes too far.
     */
    static void sliceOffset() {
        ByteBuffer parent = ByteBuffer.allocateDirect(64);
        for (int i = 0; i < 64; i++) {
            parent.put(i, (byte) i);
        }
        ByteBuffer sl = parent.slice(8, 16);
        checkEquals(8, sl.get(0), "slice element 0 is parent element 8");
        checkEquals(23, sl.get(15), "slice element 15 is parent element 23");
        sl.put(1, (byte) 99);
        checkEquals(99, parent.get(9), "write through slice is visible in parent");

        ByteBuffer dup = parent.duplicate();
        checkEquals(0, dup.get(0), "duplicate shares element 0");
        dup.put(0, (byte) 77);
        checkEquals(77, parent.get(0), "write through duplicate is visible in parent");
    }

    /** 3. get(int) is bounded by LIMIT, which can be well below capacity. */
    static void limitNotCapacity() {
        final ByteBuffer b = ByteBuffer.allocateDirect(64);
        b.put(40, (byte) 7);
        checkEquals(7, b.get(40), "in-bounds read before limit shrink");
        b.limit(10);
        checkThrows(IndexOutOfBoundsException.class,
                () -> b.get(40), "get(40) past a limit of 10");
        checkThrows(IndexOutOfBoundsException.class,
                () -> b.put(40, (byte) 1), "put(40) past a limit of 10");
        checkThrows(IndexOutOfBoundsException.class,
                () -> b.get(-1), "get(-1)");
        checkThrows(IndexOutOfBoundsException.class,
                () -> b.get(10), "get(limit)");
        b.limit(64);
        checkEquals(7, b.get(40), "the refused writes did not corrupt the byte");
    }

    /** 4. Relative accessors move `position` by exactly one. */
    static void relativeCursor() {
        ByteBuffer b = ByteBuffer.allocateDirect(8);
        checkEquals(0, b.position(), "fresh position");
        for (int i = 0; i < 8; i++) {
            ByteBuffer ret = b.put((byte) (i + 1));
            check(ret == b, "put(byte) returns this");
            checkEquals(i + 1, b.position(), "position after put " + i);
        }
        b.flip();
        for (int i = 0; i < 8; i++) {
            checkEquals(i + 1, b.get(), "relative get " + i);
            checkEquals(i + 1, b.position(), "position after get " + i);
        }
    }

    /** 4b. A relative access that throws leaves `position` untouched. */
    static void relativeCursorUnchangedOnThrow() {
        final ByteBuffer b = ByteBuffer.allocateDirect(4);
        b.position(4);
        checkThrows(BufferOverflowException.class,
                () -> b.put((byte) 1), "put(byte) at limit");
        checkEquals(4, b.position(), "position unchanged after BufferOverflow");

        b.position(0);
        b.limit(0);
        checkThrows(BufferUnderflowException.class,
                () -> b.get(), "get() at limit");
        checkEquals(0, b.position(), "position unchanged after BufferUnderflow");
    }

    /** 5. A read-only view refuses both put forms and writes nothing. */
    static void readOnly() {
        ByteBuffer src = ByteBuffer.allocateDirect(8);
        src.put(0, (byte) 42);
        final ByteBuffer ro = src.asReadOnlyBuffer();
        check(ro.isReadOnly(), "asReadOnlyBuffer is read-only");
        checkEquals(42, ro.get(0), "read-only get still works");
        checkThrows(ReadOnlyBufferException.class,
                () -> ro.put(0, (byte) 1), "read-only put(int, byte)");
        checkThrows(ReadOnlyBufferException.class,
                () -> ro.put((byte) 1), "read-only put(byte)");
        checkEquals(42, src.get(0), "read-only refusal did not write through");
    }

    /**
     * Bulk and per-element access see the same bytes — a native that reads a
     * different address than the bulk path would pass every test above in
     * isolation and still be wrong. Heap buffers run the same assertions so a
     * regression that mixes up the direct and heap layouts shows up here.
     */
    static void bulkAndElementAgree() {
        for (int direct = 0; direct < 2; direct++) {
            ByteBuffer b = direct == 1
                    ? ByteBuffer.allocateDirect(32)
                    : ByteBuffer.allocate(32);
            String tag = direct == 1 ? "direct" : "heap";
            byte[] src = new byte[32];
            for (int i = 0; i < 32; i++) {
                src[i] = (byte) (0xa0 + i);
            }
            b.position(0);
            b.put(src, 0, 32);
            b.flip();
            for (int i = 0; i < 32; i++) {
                checkEquals(src[i], b.get(i), tag + " element read after bulk put at " + i);
            }
            b.position(0);
            b.put((byte) 0x11);
            byte[] out = new byte[32];
            b.position(0);
            b.get(out, 0, 32);
            checkEquals(0x11, out[0], tag + " bulk read after element put");
            for (int i = 1; i < 32; i++) {
                checkEquals(src[i], out[i], tag + " bulk read is otherwise unchanged at " + i);
            }
        }
    }

    /**
     * 6. A derived ByteBuffer view RESETS the byte order; only a typed view
     * inherits it.
     *
     * {@code java.nio.ByteBuffer} declares {@code boolean bigEndian = true} as a
     * FIELD INITIALISER, so javac compiles that write into every {@code
     * ByteBuffer} constructor and nothing else writes it. {@code slice()},
     * {@code slice(int,int)}, {@code duplicate()} and {@code asReadOnlyBuffer()}
     * each construct a buffer, so each comes back BIG_ENDIAN however the source
     * was set — they preserve CONTENT and do NOT preserve ORDER. {@code order()}
     * is {@code public final} and reads the field directly, so there is no
     * per-subclass override point at which a VM could correct this later.
     *
     * The one thing that DOES carry the order across is {@code as<T>Buffer()},
     * and it is not an exception to the rule: the JDK compiles one concrete view
     * class per endianness ({@code ByteBufferAsIntBufferB} / {@code ...L}) and
     * freezes the order into the class at creation. That family is NOT asserted
     * here — {@code asIntBuffer} and friends are deliberately absent from the
     * forced-native list, so on a real-JDK receiver the row measures the JDK's
     * own bytecode rather than anything this campaign changed. The oracle for it
     * is {@code {direct,heap}.ord.asIntBuffer.{fromLE,fromBE}} in the probe.
     *
     * Oracle rows, all four derivations on both arms:
     * {@code {direct,heap}.ord.{fresh,afterSet,slice,sliceRange,duplicate,
     * readOnly}.order}. The content rows travel with them deliberately: a "fix"
     * that reset the order by rebuilding the view from a fresh copy would satisfy
     * every order row and fail these.
     */
    static void derivedViewOrderIsReset() {
        for (int direct = 0; direct < 2; direct++) {
            ByteBuffer b = direct == 1
                    ? ByteBuffer.allocateDirect(16)
                    : ByteBuffer.allocate(16);
            String tag = direct == 1 ? "direct" : "heap";
            for (int i = 0; i < 16; i++) {
                b.put(i, (byte) (i * 7));
            }
            // Bytes 0..3 are 0,7,14,21 = 0x00070E15 = 462357 read big-endian,
            // 0x150E0700 = 353240832 read little-endian. The same four bytes read
            // both ways, so a native that ignores the order fails exactly one.
            checkOrder("BIG_ENDIAN", b, tag + " a fresh buffer is BIG_ENDIAN");
            checkEquals(462357, b.getInt(0), tag + " getInt(0) BIG_ENDIAN");
            b.order(ByteOrder.LITTLE_ENDIAN);
            checkOrder("LITTLE_ENDIAN", b, tag + " order(LITTLE_ENDIAN) took effect");
            checkEquals(353240832, b.getInt(0), tag + " getInt(0) LITTLE_ENDIAN");

            ByteBuffer sl = b.slice();
            checkOrder("BIG_ENDIAN", sl, tag + " slice() RESETS the order");
            checkEquals(462357, sl.getInt(0), tag + " slice() reads its own BIG_ENDIAN");
            checkEquals(35, sl.get(5), tag + " slice() preserves content");

            ByteBuffer sr = b.slice(2, 4);
            checkOrder("BIG_ENDIAN", sr, tag + " slice(int,int) RESETS the order");
            checkEquals(4, sr.capacity(), tag + " slice(int,int) capacity");
            checkEquals(14, sr.get(0), tag + " slice(int,int) preserves content");

            ByteBuffer dp = b.duplicate();
            checkOrder("BIG_ENDIAN", dp, tag + " duplicate() RESETS the order");
            checkEquals(35, dp.get(5), tag + " duplicate() preserves content");

            ByteBuffer ro = b.asReadOnlyBuffer();
            checkOrder("BIG_ENDIAN", ro, tag + " asReadOnlyBuffer() RESETS the order");
            checkEquals(35, ro.get(5), tag + " asReadOnlyBuffer() preserves content");

            // The source is still LITTLE_ENDIAN: resetting the derived view must
            // not reach back into the parent, and re-setting it must still work
            // after four views have been taken. These two are the
            // over-correction arm — a "fix" that hard-codes BIG_ENDIAN
            // everywhere satisfies every row above and fails both of these.
            checkOrder("LITTLE_ENDIAN", b, tag + " the source keeps its own order");
            checkEquals(353240832, b.getInt(0), tag + " the source still reads LITTLE_ENDIAN");
            b.order(ByteOrder.BIG_ENDIAN);
            checkOrder("BIG_ENDIAN", b, tag + " the source can be set back");
            checkEquals(462357, b.getInt(0), tag + " the source reads BIG_ENDIAN again");
        }
    }

    /**
     * 7. {@code arrayOffset()} obeys the same storage-then-mutability fork as
     * {@code array()}.
     *
     * The real body is three lines — {@code if (hb == null) throw new
     * UnsupportedOperationException(); if (isReadOnly) throw new
     * ReadOnlyBufferException(); return offset;} — and the two throws are the
     * whole of what is easy to miss, because the happy path is a plain field
     * read that a VM gets right by accident. Oracle rows:
     * {@code direct.arrayOffset.throws}, {@code direct.win.arrayOffset.throws},
     * {@code direct.win.readOnly.arrayOffset.throws} (all
     * UnsupportedOperationException), {@code heap.arrayOffset = 0},
     * {@code heap.win.arrayOffset = 4} and
     * {@code heap.win.readOnly.arrayOffset.throws} (ReadOnlyBufferException).
     *
     * The WINDOW rows are the ones that cannot be faked. On a FRESH heap buffer
     * {@code arrayOffset()} is 0 and {@code array().length} is the capacity, so a
     * native that answers {@code array()} with a fresh copy of the right size and
     * {@code arrayOffset()} with a constant 0 passes every other check here. A
     * heap {@code slice()} separates them: the array is the parent's, still 16
     * long, and the offset is 4.
     *
     * {@code hasArray()} is {@code hb != null && !isReadOnly}, so BOTH read-only
     * arms answer false — including the heap one, whose array exists — and the
     * two then differ in WHICH exception they raise. That split is measured, not
     * derived, and getting it backwards is a one-word error that a
     * "did it throw at all" check cannot see; hence {@code checkThrowsExactly}.
     */
    static void arrayOffsetContract() {
        // --- heap, writable: the offset, and the window that proves it real ---
        ByteBuffer h = ByteBuffer.allocate(16);
        for (int i = 0; i < 16; i++) {
            h.put(i, (byte) (i * 7));
        }
        check(h.hasArray(), "a heap buffer hasArray");
        checkEquals(16, h.array().length, "a heap buffer's array is its capacity");
        checkEquals(0, h.arrayOffset(), "a fresh heap buffer's arrayOffset is 0");

        h.position(4);
        ByteBuffer hw = h.slice();
        checkEquals(12, hw.capacity(), "a heap window's capacity");
        check(hw.hasArray(), "a heap window hasArray");
        checkEquals(16, hw.array().length, "a heap window's array is the PARENT's 16 elements");
        check(hw.array() == h.array(), "a heap window's array is the parent's, by identity");
        checkEquals(4, hw.arrayOffset(), "a heap window's arrayOffset is 4");
        checkEquals(28, hw.array()[hw.arrayOffset()],
                "the window's byte 0 reached through array()+arrayOffset()");
        checkEquals(28, hw.get(0), "the window's byte 0 reached through get(0)");

        // --- direct: BOTH accessors refuse, with the storage-kind exception ---
        final ByteBuffer d = ByteBuffer.allocateDirect(16);
        for (int i = 0; i < 16; i++) {
            d.put(i, (byte) (i * 7));
        }
        check(!d.hasArray(), "a direct buffer does not hasArray");
        checkThrowsExactly(UnsupportedOperationException.class,
                () -> d.array(), "direct array()");
        checkThrowsExactly(UnsupportedOperationException.class,
                () -> d.arrayOffset(), "direct arrayOffset()");

        d.position(4);
        final ByteBuffer dw = d.slice();
        checkEquals(12, dw.capacity(), "a direct window's capacity");
        check(!dw.hasArray(), "a direct window does not hasArray");
        checkThrowsExactly(UnsupportedOperationException.class,
                () -> dw.array(), "direct window array()");
        checkThrowsExactly(UnsupportedOperationException.class,
                () -> dw.arrayOffset(), "direct window arrayOffset()");
        checkEquals(28, dw.get(0), "the direct window still reads its byte 0");

        // --- read-only splits the two ways, and only on the heap arm ---
        final ByteBuffer hro = hw.asReadOnlyBuffer();
        check(!hro.hasArray(), "a heap read-only view does not hasArray");
        checkThrowsExactly(ReadOnlyBufferException.class,
                () -> hro.array(), "heap read-only array()");
        checkThrowsExactly(ReadOnlyBufferException.class,
                () -> hro.arrayOffset(), "heap read-only arrayOffset()");
        checkEquals(28, hro.get(0), "the heap read-only view still reads its byte 0");

        final ByteBuffer dro = dw.asReadOnlyBuffer();
        check(!dro.hasArray(), "a direct read-only view does not hasArray");
        checkThrowsExactly(UnsupportedOperationException.class,
                () -> dro.array(), "direct read-only array()");
        checkThrowsExactly(UnsupportedOperationException.class,
                () -> dro.arrayOffset(), "direct read-only arrayOffset()");
        checkEquals(28, dro.get(0), "the direct read-only view still reads its byte 0");
    }
}
