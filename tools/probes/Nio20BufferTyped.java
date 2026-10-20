// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio20 wave 6, lane L1: typed ByteBuffer accesses (heap and direct, both byte
// orders, int and long, loop and single-access shapes) in compiled code.
// Exercises the two wave-6 switches:
//   CRATONVM_JIT_IR_BUFFER_TRAP_MISS   a profile-guarded `ByteBuffer.put*/get*`
//                                       splice deopts on a miss instead of
//                                       calling (the receiver is exact past the
//                                       test, the loop loses its opaque call);
//   CRATONVM_JIT_IR_PLAIN_STATIC_READ  `getstatic SCOPED_MEMORY_ACCESS` (one per
//                                       access) is a static-cell read for the
//                                       alias model, not the top of the lattice.
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes): every
//          kernel run REPS times so the optimizing tier compiles it, then the
//          checksum; bounds / read-only / overflow exceptions (class and
//          message) from compiled sites; and a PROFILE BREAK: each kernel is
//          warmed on one buffer class only and then handed the others (direct,
//          read-only, a slice, little-endian), which is exactly the miss a
//          trap-form site deopts on.
//   rows   `<row> <N> ns/op` lines, one op = 16 accesses unless noted:
//          heapget heapput heaplong directget directput directlong le be
//          single (one relative putInt + getInt) straight (8 unrolled relative
//          putInt, then 8 getInt).
//
//   java Nio20BufferTyped conf
//   java -Dnio20.rows=heapget,heapput,heaplong,directget,directput,directlong,le,be,single,straight Nio20BufferTyped rows
//
// A/B (cratonvm, --jdk-only and --compatible): default vs
// CRATONVM_JIT_IR_BUFFER_TRAP_MISS=0, vs CRATONVM_JIT_IR_PLAIN_STATIC_READ=0,
// vs both =0. conf must be identical in every arm and equal HotSpot's.
// CRATONVM_DBG_JITC=1 prints `ir-guard-trap-miss at pc=.. planned` for each
// buffer site the rule took.

import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class Nio20BufferTyped {
    static final int REPS = 20_000;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio20.rows",
                    "heapget,heapput,heaplong,directget,directput,directlong,le,be,single,straight"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- kernels

    // nio21 wave 1 L3: the absolute kernels `clear()` first. An absolute index
    // is checked against the LIMIT, and `single` / `straight` / `relInts` leave
    // the limit at 4 / 32 / 64 after their `flip()`, so without it the second
    // round's `getInt(32)` threw IndexOutOfBoundsException at the warm loop on
    // HotSpot too (the nio20 w6 reference was the exception, not the answers).
    static int getInts(ByteBuffer b) {
        b.clear();
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k);
        return h;
    }

    static int putInts(ByteBuffer b, int seed) {
        b.clear();
        for (int k = 0; k < 16; k++) b.putInt(4 * k, seed + k);
        return b.getInt(60);
    }

    static long longs(ByteBuffer b, long seed) {
        b.clear();
        for (int k = 0; k < 8; k++) b.putLong(seed * 31 + k);
        b.flip();
        long h = 0;
        for (int k = 0; k < 8; k++) h = 31 * h + b.getLong();
        return h;
    }

    static int relInts(ByteBuffer b, int seed) {
        b.clear();
        for (int k = 0; k < 16; k++) b.putInt(seed + k);
        b.flip();
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt();
        return h;
    }

    static int single(ByteBuffer b, int seed) {
        b.clear();
        b.putInt(seed);
        b.flip();
        return b.getInt();
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
        b.flip();
        return b.getInt() + 3 * b.getInt() + 5 * b.getInt() + 7 * b.getInt()
                + 11 * b.getInt() + 13 * b.getInt() + 17 * b.getInt() + 19 * b.getInt();
    }

    // Separate copies for the bench rows, so each row's sites carry their own
    // receiver profile (heap or direct, one byte order).
    static int getIntsHeap(ByteBuffer b) { int h = 0; for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k); return h; }
    static int getIntsDirect(ByteBuffer b) { int h = 0; for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k); return h; }
    static int putIntsHeap(ByteBuffer b, int s) { for (int k = 0; k < 16; k++) b.putInt(4 * k, s + k); return s; }
    static int putIntsDirect(ByteBuffer b, int s) { for (int k = 0; k < 16; k++) b.putInt(4 * k, s + k); return s; }
    static long longsHeap(ByteBuffer b, long s) { long h = 0; for (int k = 0; k < 8; k++) { b.putLong(8 * k, s + k); h += b.getLong(8 * k); } return h; }
    static long longsDirect(ByteBuffer b, long s) { long h = 0; for (int k = 0; k < 8; k++) { b.putLong(8 * k, s + k); h += b.getLong(8 * k); } return h; }
    static int relIntsLe(ByteBuffer b, int s) { return relInts2(b, s); }
    static int relIntsBe(ByteBuffer b, int s) { return relInts3(b, s); }
    static int relInts2(ByteBuffer b, int seed) {
        b.clear();
        for (int k = 0; k < 16; k++) b.putInt(seed + k);
        b.flip();
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt();
        return h;
    }
    static int relInts3(ByteBuffer b, int seed) {
        b.clear();
        for (int k = 0; k < 16; k++) b.putInt(seed + k);
        b.flip();
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt();
        return h;
    }

    // ---------------------------------------------------------------- conf

    static ByteBuffer filled(ByteBuffer b) {
        for (int i = 0; i < b.capacity(); i++) b.put(i, (byte) (i * 7 + 3));
        return b;
    }

    static String exc(Runnable r) {
        try {
            r.run();
            return "no exception";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static void conf() {
        ByteBuffer heap = filled(ByteBuffer.allocate(64));
        ByteBuffer heapLe = filled(ByteBuffer.allocate(64)).order(ByteOrder.LITTLE_ENDIAN);
        ByteBuffer direct = filled(ByteBuffer.allocateDirect(64));
        ByteBuffer directLe = filled(ByteBuffer.allocateDirect(64)).order(ByteOrder.LITTLE_ENDIAN);

        // 1. Each kernel warmed on the heap big-endian buffer only.
        long h = 0;
        for (int i = 0; i < REPS; i++) {
            h = h * 31 + getInts(heap);
            h = h * 31 + putInts(heap, i);
            h = h * 31 + longs(heap, i);
            h = h * 31 + relInts(heap, i);
            h = h * 31 + single(heap, i);
            h = h * 31 + straight(heap, i);
        }
        System.out.println("heap be warm " + h);
        System.out.println("heap be bytes " + getInts(heap));

        // 2. The profile break: the same compiled sites see every other class.
        ByteBuffer[] others = {
            heapLe, direct, directLe,
            filled(ByteBuffer.allocate(80)).position(8).slice(),
            filled(ByteBuffer.allocateDirect(80)).position(16).slice().order(ByteOrder.LITTLE_ENDIAN),
        };
        String[] names = {"heap le", "direct be", "direct le", "heap slice", "direct slice le"};
        for (int j = 0; j < others.length; j++) {
            ByteBuffer b = others[j];
            long g = 0;
            for (int i = 0; i < 200; i++) {
                g = g * 31 + getInts(b);
                g = g * 31 + putInts(b, i);
                g = g * 31 + longs(b, i);
                g = g * 31 + relInts(b, i);
                g = g * 31 + single(b, i);
                g = g * 31 + straight(b, i);
                // Back to the warm class in between: the de-spec path.
                g = g * 31 + getInts(heap);
            }
            System.out.println(names[j] + " " + g + " bytes " + getInts(b));
        }

        // 3. Read-only buffers: reads work, writes throw from the compiled site.
        ByteBuffer ro = filled(ByteBuffer.allocate(64)).asReadOnlyBuffer();
        ByteBuffer dro = filled(ByteBuffer.allocateDirect(64)).asReadOnlyBuffer();
        System.out.println("ro get " + getInts(ro) + " " + getInts(dro));
        System.out.println("ro put " + exc(() -> putInts(ro, 1)));
        System.out.println("direct ro put " + exc(() -> putInts(dro, 1)));
        System.out.println("ro rel " + exc(() -> relInts(ro, 1)));

        // 4. Bounds from compiled sites (absolute and relative).
        ByteBuffer small = filled(ByteBuffer.allocate(16));
        ByteBuffer dsmall = filled(ByteBuffer.allocateDirect(16));
        System.out.println("abs get oob " + exc(() -> getInts(small)));
        System.out.println("abs put oob " + exc(() -> putInts(small, 2)));
        System.out.println("direct abs get oob " + exc(() -> getInts(dsmall)));
        System.out.println("direct abs put oob " + exc(() -> putInts(dsmall, 2)));
        System.out.println("rel put overflow " + exc(() -> relInts(small, 3)));
        System.out.println("rel long overflow " + exc(() -> longs(dsmall, 3)));
        System.out.println("neg index " + exc(() -> heap.getInt(-1)));
        System.out.println("tail index " + exc(() -> heap.getInt(61)));
        System.out.println("tail long " + exc(() -> heap.getLong(57)));
        System.out.println("after oob " + getInts(heap) + " " + single(heap, 5));

        // 5. Byte order is honoured by every width.
        ByteBuffer bo = ByteBuffer.allocate(16);
        bo.putInt(0, 0x01020304);
        bo.putLong(4, 0x0102030405060708L);
        bo.putShort(12, (short) 0x0102);
        bo.putChar(14, (char) 0xA1B2);
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 16; i++) sb.append(String.format("%02x", bo.get(i)));
        System.out.println("be bytes " + sb);
        bo.order(ByteOrder.LITTLE_ENDIAN);
        System.out.println("le reads " + Integer.toHexString(bo.getInt(0)) + " "
                + Long.toHexString(bo.getLong(4)) + " " + Integer.toHexString(bo.getShort(12) & 0xffff)
                + " " + Integer.toHexString(bo.getChar(14)));
    }

    // ---------------------------------------------------------------- rows

    static void rows(String which) {
        ByteBuffer heap = filled(ByteBuffer.allocate(64));
        ByteBuffer direct = filled(ByteBuffer.allocateDirect(64));
        ByteBuffer heapLe = filled(ByteBuffer.allocate(64)).order(ByteOrder.LITTLE_ENDIAN);
        ByteBuffer heapBe = filled(ByteBuffer.allocate(64));
        ByteBuffer heapSingle = filled(ByteBuffer.allocate(64));
        ByteBuffer heapStraight = filled(ByteBuffer.allocate(64));
        long sink = 0;
        for (String row : which.split(",")) {
            int n = 2_000_000;
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (row) {
                    case "heapget" -> { for (int i = 0; i < n; i++) sink += getIntsHeap(heap); }
                    case "heapput" -> { for (int i = 0; i < n; i++) sink += putIntsHeap(heap, i); }
                    case "heaplong" -> { for (int i = 0; i < n; i++) sink += longsHeap(heap, i); }
                    case "directget" -> { for (int i = 0; i < n; i++) sink += getIntsDirect(direct); }
                    case "directput" -> { for (int i = 0; i < n; i++) sink += putIntsDirect(direct, i); }
                    case "directlong" -> { for (int i = 0; i < n; i++) sink += longsDirect(direct, i); }
                    case "le" -> { for (int i = 0; i < n; i++) sink += relIntsLe(heapLe, i); }
                    case "be" -> { for (int i = 0; i < n; i++) sink += relIntsBe(heapBe, i); }
                    case "single" -> { for (int i = 0; i < n; i++) sink += single(heapSingle, i); }
                    case "straight" -> { for (int i = 0; i < n; i++) sink += straight(heapStraight, i); }
                    default -> throw new IllegalArgumentException(row);
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(row + " " + (best / n) + " ns/op");
        }
        System.out.println("sink " + (sink & 0xff));
    }
}
