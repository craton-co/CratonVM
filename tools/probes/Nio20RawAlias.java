// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio20 wave 4, lane L1: the optimizing tier's alias model around the
// `ScopedMemoryAccess` fold (`CRATONVM_JIT_IR_SCOPED_RAW_ALIAS`, W3L4-1: the
// fold is RAW memory -- it may overwrite any primitive array element, never a
// field) and the builder's field memo inside spliced bodies
// (`CRATONVM_JIT_IR_FIELD_MEMO_IN_SPLICES`, W3L4-2).
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes). Each
//          kernel interleaves a ByteBuffer write with a read the optimizer
//          could wrongly reuse across it: the backing byte[] element itself
//          (must see the write), a long[]/int[] view of other memory (must not
//          change), an object field written between two buffer writes (must see
//          its own store), the buffer's own position/limit around relative
//          writes, and a direct buffer (the off-heap arm). Each runs REPS times
//          so the optimizing tier compiles it.
//   rows   `<row> <N> ns/op` lines: heapbb (16 putInt + 16 getInt, R17NioRows'
//          loop), fieldmix (a field read and write around every putInt),
//          straight (8 unrolled relative putInt calls, the memo-in-splice shape).
//
//   java Nio20RawAlias conf
//   java -Dnio20.rows=heapbb,fieldmix,straight Nio20RawAlias rows
//
// A/B (cratonvm): CRATONVM_JIT_IR_SCOPED_RAW_ALIAS=0, and separately
// CRATONVM_JIT_IR_FIELD_MEMO_IN_SPLICES=0; conf must be identical in every arm.

import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class Nio20RawAlias {
    static final int REPS = 20_000;

    static final class Holder {
        int counter;
        int[] ints = new int[4];
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio20.rows", "heapbb,fieldmix,straight"));
        } else {
            conf();
        }
    }

    // ---------------------------------------------------------------- conf

    /// The byte[] behind the buffer must observe every putInt.
    static long backingArray(byte[] a, ByteBuffer b, int seed) {
        long h = 0;
        for (int i = 0; i < 16; i++) {
            int before = a[4];
            b.putInt(4, seed + i);
            int after = a[4];
            h = h * 31 + before;
            h = h * 31 + after;
            h = h * 31 + a[7];
        }
        return h;
    }

    /// A field written between two buffer writes keeps its own value, and an
    /// int[] elsewhere is untouched.
    static long fieldAround(Holder o, ByteBuffer b, int seed) {
        long h = 0;
        for (int i = 0; i < 16; i++) {
            int c0 = o.counter;
            b.putInt(0, c0 + seed);
            o.counter = c0 + 1;
            b.putInt(4, o.counter);
            h = h * 31 + o.counter;
            h = h * 31 + b.getInt(4);
            h = h * 31 + o.ints[i & 3];
            o.ints[i & 3] += b.get(0);
        }
        return h;
    }

    /// Relative writes advance position; the reads of position/limit around
    /// them must see each advance.
    static long relativeCursor(ByteBuffer b, int seed) {
        b.clear();
        long h = 0;
        while (b.remaining() >= 4) {
            int p = b.position();
            b.putInt(seed ^ p);
            h = h * 31 + (b.position() - p);
            h = h * 31 + b.limit();
        }
        b.flip();
        while (b.hasRemaining()) {
            h = h * 31 + b.getInt();
        }
        return h;
    }

    static long repeat(java.util.function.LongSupplier f) {
        long h = 0;
        for (int r = 0; r < REPS / 100; r++) {
            h = h * 1_000_003 + f.getAsLong();
        }
        return h;
    }

    static void line(String label, java.util.function.Supplier<Object> f) {
        String out;
        try {
            out = String.valueOf(f.get());
        } catch (Throwable t) {
            out = "threw " + t.getClass().getName();
        }
        System.out.println(label + " " + out);
    }

    static void conf() {
        byte[] a = new byte[64];
        ByteBuffer be = ByteBuffer.wrap(a);
        ByteBuffer le = ByteBuffer.wrap(new byte[64]).order(ByteOrder.LITTLE_ENDIAN);
        ByteBuffer direct = ByteBuffer.allocateDirect(64);
        Holder o = new Holder();
        for (int r = 0; r < REPS; r++) {
            backingArray(a, be, r);
            fieldAround(o, le, r);
            relativeCursor(direct, r);
        }
        line("backing", () -> repeat(() -> backingArray(a, be, 3)));
        line("backing-le", () -> {
            byte[] x = new byte[64];
            ByteBuffer y = ByteBuffer.wrap(x).order(ByteOrder.LITTLE_ENDIAN);
            return repeat(() -> backingArray(x, y, 5));
        });
        line("field", () -> {
            Holder h = new Holder();
            return repeat(() -> fieldAround(h, le, 7));
        });
        line("field-direct", () -> {
            Holder h = new Holder();
            return repeat(() -> fieldAround(h, direct, 9));
        });
        line("cursor-heap", () -> repeat(() -> relativeCursor(ByteBuffer.allocate(64), 11)));
        line("cursor-direct", () -> repeat(() -> relativeCursor(direct, 13)));
        line("cursor-slice", () -> repeat(() -> relativeCursor(ByteBuffer.wrap(new byte[80], 8, 64).slice(), 15)));
    }

    // ---------------------------------------------------------------- rows

    static final ByteBuffer HB = ByteBuffer.allocate(256);
    static final Holder H = new Holder();

    static int heapBb(ByteBuffer b, int i) {
        b.clear();
        for (int k = 0; k < 16; k++) b.putInt(i + k);
        b.flip();
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt();
        return h;
    }

    static int fieldMix(Holder o, ByteBuffer b, int i) {
        int h = 0;
        for (int k = 0; k < 16; k++) {
            b.putInt(4 * k, o.counter + k);
            o.counter += i;
            h += o.counter;
        }
        return h + b.getInt(8);
    }

    static int straight(ByteBuffer b, int i) {
        b.clear();
        b.putInt(i);
        b.putInt(i + 1);
        b.putInt(i + 2);
        b.putInt(i + 3);
        b.putInt(i + 4);
        b.putInt(i + 5);
        b.putInt(i + 6);
        b.putInt(i + 7);
        return b.position();
    }

    static void rows(String which) {
        long sink = 0;
        for (String row : which.split(",")) {
            int n = 2_000_000;
            long best = Long.MAX_VALUE;
            for (int warm = 0; warm < 5; warm++) {
                long t0 = System.nanoTime();
                for (int i = 0; i < n; i++) {
                    switch (row) {
                        case "heapbb" -> sink += heapBb(HB, i);
                        case "fieldmix" -> sink += fieldMix(H, HB, i);
                        case "straight" -> sink += straight(HB, i);
                        default -> throw new IllegalArgumentException(row);
                    }
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(row + " " + (best / n) + " ns/op");
        }
        System.out.println("sink " + (sink & 0xff));
    }
}
