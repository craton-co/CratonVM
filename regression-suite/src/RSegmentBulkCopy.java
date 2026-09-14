// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/**
 * {@code MemorySegment.copy(src, srcOffset, dst, dstOffset, byteCount)} —
 * the five-argument bulk form — with an ALIASING heap segment on one or
 * both sides, and {@code copy(src, layout, srcOffset, array, index,
 * count)} into an {@code int[]}.
 *
 * WHY THIS VECTOR EXISTS.
 *
 * The bulk copy was a raw pointer memmove between two
 * {@code segment_address} results. An aliasing heap segment — what
 * {@code MemorySegment.ofArray(byte[]/short[]/char[])} returns — has no
 * address: its payload is a Java array and its pointer field is 0. The
 * implementation guarded the memmove with a null check on both
 * addresses and, when either was null, DID NOTHING AND RETURNED
 * NORMALLY.
 *
 * That shape is why every assertion here is about CONTENT and why the
 * fixtures never contain a zero. A vector that copied zeros, or that
 * only checked "no exception was thrown", passes against a copy that
 * does not copy. So does one that spot-checks a single element: the
 * failure is uniform, but a fixture whose first element is legitimately
 * 0 hides it at exactly the index a spot check would use.
 *
 * WHAT IS DELIBERATELY NOT ASSERTED.
 *
 * {@code ofArray} over {@code int[]}, {@code long[]}, {@code float[]}
 * and {@code double[]} does NOT alias on CratonVM. Those four allocate
 * an off-heap mirror and copy the array into it, on purpose, because
 * the carriers exist to be handed to downcalls and a moving Java array
 * cannot be. The asymmetry is stated in
 * {@code native-builtins/src/panama.rs} beside the registrations. So a
 * copy INTO one of those mirrors is expected to leave the Java array
 * alone, and this file does not pretend otherwise — the four
 * mirror-backed widths are exercised through the layout overload
 * instead, which names the destination array directly and is the form
 * a caller should use to fill an {@code int[]} from a segment.
 */
public class RSegmentBulkCopy {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void checkEquals(long expected, long actual, String m) {
        checks++;
        if (expected != actual) {
            throw new AssertionError(m + ": expected " + expected + " got " + actual);
        }
    }

    /** Never zero, so "did nothing" cannot masquerade as "copied". */
    static byte b(int i) {
        return (byte) ((i * 37) | 1);
    }

    static void nativeToAliasingHeap(Arena arena) {
        final int n = 1024;
        MemorySegment src = arena.allocate(n, 8);
        for (int i = 0; i < n; i++) {
            src.set(ValueLayout.JAVA_BYTE, i, b(i));
        }
        byte[] dst = new byte[n];
        MemorySegment.copy(src, 0L, MemorySegment.ofArray(dst), 0L, n);
        int bad = 0;
        for (int i = 0; i < n; i++) {
            if (dst[i] != b(i)) {
                bad++;
            }
        }
        checkEquals(0, bad, "native -> aliasing heap byte[] copied every byte");
    }

    static void aliasingHeapToNative(Arena arena) {
        final int n = 1024;
        byte[] host = new byte[n];
        for (int i = 0; i < n; i++) {
            host[i] = b(i ^ 0x5A);
        }
        MemorySegment dst = arena.allocate(n, 8);
        MemorySegment.copy(MemorySegment.ofArray(host), 0L, dst, 0L, n);
        int bad = 0;
        for (int i = 0; i < n; i++) {
            if (dst.get(ValueLayout.JAVA_BYTE, i) != host[i]) {
                bad++;
            }
        }
        checkEquals(0, bad, "aliasing heap byte[] -> native copied every byte");
    }

    static void aliasingHeapToAliasingHeap() {
        final int n = 512;
        byte[] from = new byte[n];
        byte[] to = new byte[n];
        for (int i = 0; i < n; i++) {
            from[i] = b(i);
        }
        MemorySegment.copy(MemorySegment.ofArray(from), 0L, MemorySegment.ofArray(to), 0L, n);
        int bad = 0;
        for (int i = 0; i < n; i++) {
            if (to[i] != from[i]) {
                bad++;
            }
        }
        checkEquals(0, bad, "aliasing heap -> aliasing heap copied every byte");
    }

    static void nativeToNative(Arena arena) {
        final int n = 256;
        MemorySegment a = arena.allocate(n, 8);
        MemorySegment c = arena.allocate(n, 8);
        for (int i = 0; i < n; i++) {
            a.set(ValueLayout.JAVA_BYTE, i, b(i));
        }
        MemorySegment.copy(a, 0L, c, 0L, n);
        int bad = 0;
        for (int i = 0; i < n; i++) {
            if (c.get(ValueLayout.JAVA_BYTE, i) != b(i)) {
                bad++;
            }
        }
        checkEquals(0, bad, "native -> native copied every byte");
    }

    /**
     * A range that starts and ends inside a {@code short[]} element.
     * The bytes outside the range must survive untouched — the hazard
     * of a read-modify-write fallback is that it writes back a stale
     * copy of its neighbours.
     */
    static void unalignedIntoShortArray(Arena arena) {
        short[] dst = new short[4];
        java.util.Arrays.fill(dst, (short) 0x1122);
        MemorySegment src = arena.allocate(4, 8);
        for (int i = 0; i < 4; i++) {
            src.set(ValueLayout.JAVA_BYTE, i, (byte) (0xA0 + i));
        }
        // Bytes 3..6: the tail of element 1, all of element 2, the head
        // of element 3.
        MemorySegment.copy(src, 0L, MemorySegment.ofArray(dst), 3L, 4L);
        checkEquals((short) 0x1122, dst[0], "element before the range is untouched");
        // Little-endian: byte 3 is element 1's high byte.
        checkEquals((short) 0xA022, dst[1], "element 1 keeps its low byte");
        checkEquals((short) 0xA2A1, dst[2], "element 2 takes both bytes");
        checkEquals((short) 0x11A3, dst[3], "element 3 keeps its high byte");
    }

    /** The layout overload, which names the destination array itself. */
    static void layoutOverloadIntoIntArray(Arena arena) {
        final int n = 777;
        MemorySegment src = arena.allocate((long) n * 4L, 8);
        for (int i = 0; i < n; i++) {
            src.set(ValueLayout.JAVA_INT, (long) i * 4L, (i * 0x9E3779B9) | 1);
        }
        int[] dst = new int[n + 3];
        MemorySegment.copy(src, ValueLayout.JAVA_INT, 0L, dst, 2, n);
        checkEquals(0, dst[0], "the destination prefix is untouched");
        checkEquals(0, dst[1], "the destination prefix is untouched");
        int bad = 0;
        for (int i = 0; i < n; i++) {
            if (dst[2 + i] != ((i * 0x9E3779B9) | 1)) {
                bad++;
            }
        }
        checkEquals(0, bad, "the layout overload filled every int");
        checkEquals(0, dst[n + 2], "the destination suffix is untouched");
    }

    /** A copy of zero bytes is a no-op, not an error. */
    static void emptyCopy(Arena arena) {
        MemorySegment src = arena.allocate(8, 8);
        byte[] dst = new byte[2];
        dst[0] = 7;
        MemorySegment.copy(src, 0L, MemorySegment.ofArray(dst), 0L, 0L);
        checkEquals(7, dst[0], "a zero-byte copy changes nothing");
    }

    /** Out of range must throw rather than copy a truncated prefix. */
    static void outOfRangeThrows(Arena arena) {
        MemorySegment src = arena.allocate(8, 8);
        byte[] dst = new byte[4];
        boolean threw = false;
        try {
            MemorySegment.copy(src, 0L, MemorySegment.ofArray(dst), 0L, 8L);
        } catch (RuntimeException e) {
            threw = true;
        }
        check(threw, "a copy past the destination's end throws");
    }

    public static void main(String[] args) {
        try (Arena arena = Arena.ofConfined()) {
            nativeToNative(arena);
            nativeToAliasingHeap(arena);
            aliasingHeapToNative(arena);
            aliasingHeapToAliasingHeap();
            unalignedIntoShortArray(arena);
            layoutOverloadIntoIntArray(arena);
            emptyCopy(arena);
            outOfRangeThrows(arena);
        }
        System.out.println("CK RSegmentBulkCopy checks=" + checks);
        System.out.println("PASS RSegmentBulkCopy (" + checks + " checks)");
    }
}
