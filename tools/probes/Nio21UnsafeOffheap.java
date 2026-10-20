// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 4, lane L3 (proposal W3L3-2): `sun.misc.Unsafe` primitive
// accessors with a NULL base (an off-heap address from `allocateMemory`, the
// Netty `PlatformDependent` shape), which the optimizing tier's `Unsafe`
// prefix now serves inline (`CRATONVM_JIT_IR_UNSAFE_OFFHEAP_PREFIX`), as the
// single-pass prefix has since nio21 wave 3
// (`CRATONVM_JIT_SP_UNSAFE_OFFHEAP_PREFIX`).
//
//   conf   deterministic text, to diff against HotSpot 25: every width read
//          and written at every in-block offset (unaligned included), the
//          two-argument `(null, address)` and the one-argument `(address)`
//          forms, folded over REPS repetitions so the loops compile.
//   rows   `<row> <N> ns/op` lines (one op = 16 accesses):
//          offget (getInt), offput (putInt), offlong (putLong + getLong).
//
//   java Nio21UnsafeOffheap conf
//   java -Dnio21.rows=offget,offput,offlong Nio21UnsafeOffheap rows
//
// A/B arms (cratonvm): CRATONVM_JIT_IR_UNSAFE_OFFHEAP_PREFIX=0 (the optimizing
// arm off), CRATONVM_JIT_SP_UNSAFE_OFFHEAP_PREFIX=0 (the single-pass arm off).
// With CRATONVM_DBG_JITC=1 the optimizing tier prints
// `ir-unsafe-inline n<id> off-heap arm (arena table: ..)` per site.

import java.lang.reflect.Field;

public class Nio21UnsafeOffheap {
    static final sun.misc.Unsafe U;

    static {
        try {
            Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
            f.setAccessible(true);
            U = (sun.misc.Unsafe) f.get(null);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static final int REPS = 20_000;
    static final int BLOCK = 64;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows", "offget,offput,offlong"));
        } else {
            conf();
        }
    }

    // ---------------------------------------------------------------- conf

    static void fill(long a) {
        for (int i = 0; i < BLOCK; i++) {
            U.putByte(null, a + i, (byte) (i * 37 + 11));
        }
    }

    static long readAll(long a) {
        long h = 0;
        for (int i = 0; i + 8 <= BLOCK; i++) {
            h = h * 31 + U.getByte(null, a + i);
            h = h * 31 + U.getShort(null, a + i);
            h = h * 31 + U.getChar(null, a + i);
            h = h * 31 + U.getInt(null, a + i);
            h = h * 31 + U.getLong(null, a + i);
            h = h * 31 + U.getInt(a + i);
            h = h * 31 + U.getLong(a + i);
        }
        return h;
    }

    static long writeAll(long a, int seed) {
        long h = 0;
        for (int i = 0; i + 8 <= BLOCK; i += 3) {
            U.putLong(null, a + i, 0x0102030405060708L * (seed + i));
            U.putInt(null, a + i + 1, 0x7f6e5d4c + seed + i);
            U.putShort(null, a + i + 2, (short) (0x8001 + seed + i));
            U.putChar(null, a + i + 3, (char) (0xfffe - seed - i));
            U.putByte(null, a + i + 4, (byte) (seed - i));
            U.putInt(a + i + 5, seed * 7 - i);
            h = h * 31 + U.getLong(null, a + i) + U.getInt(null, a + i + 1)
                    + U.getShort(null, a + i + 2) + U.getChar(null, a + i + 3);
        }
        return h;
    }

    static void conf() {
        long a = U.allocateMemory(BLOCK);
        try {
            fill(a);
            long r = 0;
            for (int k = 0; k < REPS; k++) {
                r += readAll(a);
            }
            System.out.println("read " + r);
            long w = 0;
            for (int k = 0; k < REPS; k++) {
                w += writeAll(a, k & 255);
            }
            System.out.println("write " + w);
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < BLOCK; i++) {
                sb.append(String.format("%02x", U.getByte(a + i)));
            }
            System.out.println("bytes " + sb);
        } finally {
            U.freeMemory(a);
        }
        System.out.println("done");
    }

    // ---------------------------------------------------------------- rows

    static int getInts(long a) {
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + U.getInt(null, a + 4 * k);
        return h;
    }

    static int putInts(long a, int s) {
        for (int k = 0; k < 16; k++) U.putInt(null, a + 4 * k, s + k);
        return s;
    }

    static long longs(long a, long s) {
        long h = 0;
        for (int k = 0; k < 8; k++) {
            U.putLong(null, a + 8 * k, s + k);
            h += U.getLong(null, a + 8 * k);
        }
        return h;
    }

    static void rows(String which) {
        long a = U.allocateMemory(BLOCK);
        try {
            fill(a);
            long sink = 0;
            for (String row : which.split(",")) {
                int n = 2_000_000;
                long best = Long.MAX_VALUE;
                for (int rep = 0; rep < 5; rep++) {
                    long t0 = System.nanoTime();
                    switch (row) {
                        case "offget" -> { for (int i = 0; i < n; i++) sink += getInts(a); }
                        case "offput" -> { for (int i = 0; i < n; i++) sink += putInts(a, i); }
                        case "offlong" -> { for (int i = 0; i < n; i++) sink += longs(a, i); }
                        default -> throw new IllegalArgumentException(row);
                    }
                    best = Math.min(best, System.nanoTime() - t0);
                }
                System.out.println(row + " " + (best / n) + " ns/op");
            }
            System.out.println("sink " + (sink & 0xff));
        } finally {
            U.freeMemory(a);
        }
    }
}
