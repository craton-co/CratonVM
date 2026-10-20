// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: the byte-offset field-cell screen
// (`CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS`: `SUB R11, 16 ; TEST R11B, 7 ;
// JNZ decline ; SHR R11, 3` in `direct_helpers::unsafe_inline_cell_screen_bytes`)
// driven through sun.misc.Unsafe directly.
//
// Risk targeted: the screen admits only offsets `16 + slot * 8` and must
// DECLINE (to the natives, which serve the bytes) for every offset inside a
// field (`k != 0`): `getInt(o, longOff + 4)` is the HIGH half of a long,
// `getByte(o, intOff + 1)` the second byte, `getShort(o, intOff + 2)` the
// upper half -- little-endian answers HotSpot gives on x64 for any layout.
// An arm that accepts `longOff + 4` as "the next slot" (or truncates the low
// bits) reads another field. Also: the int / long view of one long field
// (the wide bit), array element offsets through the same thin calls (the
// array arm, which must not take the field screen), a superclass field's
// offset on a subclass object, CAS / getAndAdd through raw offsets, and
// volatile get / put of int and long. Offsets are never printed (they are
// layout-dependent), only the values read.
//
// Run:
//   javac -d out R21FzUpdUnsafe1.java
//   java -Xmx256m -cp out R21FzUpdUnsafe1                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzUpdUnsafe1
//   CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 CRATONVM_JIT_UNSAFE_VOLATILE_PUBLISHES_FIELD_CELL=1 cratonvm ...
//   CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS=0 cratonvm ... (control)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.lang.reflect.Field;
import sun.misc.Unsafe;

public class R21FzUpdUnsafe1 {
    static final int WARM = 60_000;
    static final Unsafe U;
    static final long OFF_I;
    static final long OFF_L;
    static final long OFF_I2;
    static final long OFF_SUB;
    static final long INT_BASE;
    static final long INT_SCALE;
    static final long LONG_BASE;

    static class Cell {
        volatile int i;
        volatile long l;
        volatile int i2;
    }

    static final class SubCell extends Cell {
        volatile int s;
    }

    static {
        try {
            Field f = Unsafe.class.getDeclaredField("theUnsafe");
            f.setAccessible(true);
            U = (Unsafe) f.get(null);
            OFF_I = U.objectFieldOffset(Cell.class.getDeclaredField("i"));
            OFF_L = U.objectFieldOffset(Cell.class.getDeclaredField("l"));
            OFF_I2 = U.objectFieldOffset(Cell.class.getDeclaredField("i2"));
            OFF_SUB = U.objectFieldOffset(SubCell.class.getDeclaredField("s"));
            INT_BASE = U.arrayBaseOffset(int[].class);
            INT_SCALE = U.arrayIndexScale(int[].class);
            LONG_BASE = U.arrayBaseOffset(long[].class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long fieldOps(Cell c, int r) {
        long h = 0;
        U.putIntVolatile(c, OFF_I, 0x11223344 + r);
        U.putLongVolatile(c, OFF_L, 0x0102030405060708L + ((long) r << 40));
        U.putIntVolatile(c, OFF_I2, -r);
        h = h * 31 + U.getIntVolatile(c, OFF_I);
        h = h * 31 + U.getLongVolatile(c, OFF_L);
        h = h * 31 + U.getIntVolatile(c, OFF_I2);
        h = h * 31 + U.getInt(c, OFF_L);          // low half of the long
        h = h * 31 + U.getInt(c, OFF_L + 4);      // high half
        h = h * 31 + U.getByte(c, OFF_I + 1);     // second byte of the int
        h = h * 31 + U.getShort(c, OFF_I + 2);    // upper half of the int
        h = h * 31 + U.getAndAddInt(c, OFF_I, 5);
        h = h * 31 + U.getAndAddLong(c, OFF_L, 1L << 32);
        h = h * 31 + (U.compareAndSwapInt(c, OFF_I2, -r, r) ? 1 : 0);
        h = h * 31 + (U.compareAndSwapInt(c, OFF_I2, -r, r) ? 1 : 0);
        h = h * 31 + (U.compareAndSwapLong(c, OFF_L, U.getLongVolatile(c, OFF_L), 42L) ? 1 : 0);
        h = h * 31 + U.getAndSetInt(c, OFF_I, 7);
        if (c instanceof SubCell sc) {
            U.putIntVolatile(sc, OFF_SUB, r * 3);
            h = h * 31 + U.getIntVolatile(sc, OFF_SUB);
        }
        return h;
    }

    static long arrayOps(int[] a, long[] l, int r) {
        long h = 0;
        int k = r & 3;
        U.putIntVolatile(a, INT_BASE + k * INT_SCALE, r);
        h = h * 31 + U.getIntVolatile(a, INT_BASE + k * INT_SCALE);
        h = h * 31 + U.getAndAddInt(a, INT_BASE + k * INT_SCALE, 2);
        U.putLongVolatile(l, LONG_BASE + 8L * k, ~(long) r);
        h = h * 31 + U.getLongVolatile(l, LONG_BASE + 8L * k);
        h = h * 31 + U.getInt(l, LONG_BASE + 8L * k + 4);
        return h;
    }

    static String show(Cell c) {
        String s = Integer.toHexString(c.i) + "," + Long.toHexString(c.l) + "," + c.i2;
        if (c instanceof SubCell sc) {
            s += "," + sc.s;
        }
        return s;
    }

    static void rows(String tag) {
        Cell a = new Cell();
        Cell b = new SubCell();
        int[] ia = new int[4];
        long[] la = new long[4];
        System.out.println("row " + tag + " cell " + fieldOps(a, 1) + " " + show(a));
        System.out.println("row " + tag + " sub " + fieldOps(b, 2) + " " + show(b));
        System.out.println("row " + tag + " arr " + arrayOps(ia, la, 3) + " " + java.util.Arrays.toString(ia) + " "
                + java.util.Arrays.toString(la));
        a.l = 0x8877665544332211L;
        a.i = 0xCAFEBABE;
        System.out.println("row " + tag + " views " + Integer.toHexString(U.getInt(a, OFF_L)) + " "
                + Integer.toHexString(U.getInt(a, OFF_L + 4)) + " " + Integer.toHexString(U.getByte(a, OFF_I) & 0xFF)
                + " " + Integer.toHexString(U.getByte(a, OFF_I + 3) & 0xFF) + " "
                + Integer.toHexString(U.getShort(a, OFF_I + 2) & 0xFFFF));
    }

    public static void main(String[] args) {
        rows("cold");
        Cell[] cs = {new Cell(), new SubCell(), new Cell()};
        int[] ia = new int[4];
        long[] la = new long[4];
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + fieldOps(cs[r % 3], r);
            h = h * 31 + arrayOps(ia, la, r);
        }
        System.out.println("warm hash " + h + " " + show(cs[0]) + " " + show(cs[1]));
        rows("hot");
        rows("after");
    }
}
