// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.BufferOverflowException;
import java.nio.BufferUnderflowException;
import java.nio.ByteBuffer;
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
 * NOTE: unlike most of this suite, this class is expected to PASS on the
 * pre-change binary too — the natives are a throughput substitution, not a
 * bug fix, so "identical to the bytecode it replaces" IS the whole contract
 * and both arms must agree. It fails only if the substitution diverges.
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
}
