// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio20 wave 1, lane L2 (NIO19-2 / NIO19-9): the `Unsafe` primitive-array
// accessors answered inline by the optimizing tier, and the
// `Reference.reachabilityFence` keep-alive.
//
// Two halves:
//
//   conf   deterministic text, to diff against HotSpot 25. Every accessor
//          width over every primitive array kind (mismatched widths
//          included), unaligned offsets, the last in-bounds offset, the
//          big-endian ByteBuffer views, and off-heap (null base) reads. Run
//          long enough to be compiled by the optimizing tier (the loops
//          repeat each check `REPS` times and fold the answers).
//   rows   `<row> <N> ns/op` lines: heapbb (ByteBuffer.getInt/putInt over a
//          heap buffer), unsafeget / unsafeput (sun.misc.Unsafe over a
//          byte[]), unaligned (getLong at odd offsets), fence
//          (Reference.reachabilityFence in a loop).
//
//   java Nio20UnsafeAccess conf
//   java -Dnio20.rows=heapbb,unsafeget,unsafeput,unaligned,fence Nio20UnsafeAccess rows
//
// A/B arms (cratonvm): CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE=0 (NIO19-2 off) and
// CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE=0 (NIO19-9 off). With
// CRATONVM_DBG_JITC=1 the compiler names every inline site
// (`ir-unsafe-inline`, `ir-reachability-fence keep-alive`).

import java.lang.ref.Reference;
import java.lang.reflect.Field;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class Nio20UnsafeAccess {
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

    static final long BB = U.arrayBaseOffset(byte[].class);
    static final int REPS = 20_000;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio20.rows", "heapbb,unsafeget,unsafeput,unaligned,fence"));
        } else {
            conf();
        }
    }

    // ---------------------------------------------------------------- conf

    static long foldGets(Object o, long off) {
        long h = 0;
        h = h * 31 + U.getByte(o, off);
        h = h * 31 + U.getShort(o, off);
        h = h * 31 + U.getChar(o, off);
        h = h * 31 + U.getInt(o, off);
        h = h * 31 + U.getLong(o, off);
        return h;
    }

    static long getsLoop(Object o, long off) {
        long h = 0;
        for (int i = 0; i < REPS; i++) {
            h += foldGets(o, off);
        }
        return h / REPS;
    }

    static void putsLoop(Object o, long off, int width, long v) {
        for (int i = 0; i < REPS; i++) {
            switch (width) {
                case 1 -> U.putByte(o, off, (byte) v);
                case 2 -> U.putShort(o, off, (short) v);
                case 3 -> U.putChar(o, off, (char) v);
                case 4 -> U.putInt(o, off, (int) v);
                default -> U.putLong(o, off, v);
            }
        }
    }

    static void fill(byte[] b) {
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) (0x81 + i * 37);
        }
    }

    static void conf() {
        byte[] bytes = new byte[29];
        fill(bytes);
        long[] longs = {0x0102030405060708L, 0x8877665544332211L, -1L};
        int[] ints = {0x80000001, 0x7fffabcd, 0x01020304};
        char[] chars = {'￿', 'A', '老', 'z'};
        short[] shorts = {(short) 0x8000, 0x7fff, -2, 3};
        double[] doubles = {Math.PI, -0.0};
        boolean[] bools = new boolean[16];
        bools[3] = true;
        Object[][] cases = {
            {"byte[]", bytes, (long) bytes.length},
            {"long[]", longs, (long) longs.length * 8},
            {"int[]", ints, (long) ints.length * 4},
            {"char[]", chars, (long) chars.length * 2},
            {"short[]", shorts, (long) shorts.length * 2},
            {"double[]", doubles, (long) doubles.length * 8},
            {"boolean[]", bools, (long) bools.length},
        };
        for (Object[] c : cases) {
            Object arr = c[1];
            long payload = (Long) c[2];
            long base = U.arrayBaseOffset(arr.getClass());
            // Every 8-byte window that fits, at every byte offset.
            for (long d = 0; d + 8 <= payload; d++) {
                System.out.println(c[0] + " get+" + d + " " + getsLoop(arr, base + d));
            }
        }
        // Writes of every width at odd offsets, read back as bytes.
        for (int width : new int[] {1, 2, 3, 4, 8}) {
            byte[] b = new byte[24];
            fill(b);
            putsLoop(b, BB + 3, width, 0x1122334455667788L);
            putsLoop(b, BB + 13, width, -0x0102030405060708L);
            StringBuilder sb = new StringBuilder("put w" + width);
            for (byte x : b) {
                sb.append(' ').append(x);
            }
            System.out.println(sb);
        }
        // The last in-bounds window of each width over a 13-byte array.
        byte[] tail = new byte[13];
        fill(tail);
        System.out.println("tail long " + U.getLong(tail, BB + 5));
        System.out.println("tail int " + U.getInt(tail, BB + 9));
        System.out.println("tail short " + U.getShort(tail, BB + 11));
        System.out.println("tail byte " + U.getByte(tail, BB + 12));
        // ByteBuffer views: both orders, unaligned indices, the Unaligned
        // bodies under them.
        for (ByteOrder order : new ByteOrder[] {ByteOrder.BIG_ENDIAN, ByteOrder.LITTLE_ENDIAN}) {
            ByteBuffer bb = ByteBuffer.allocate(40).order(order);
            long h = 0;
            for (int r = 0; r < REPS; r++) {
                for (int i = 0; i + 8 <= 40; i += 3) {
                    bb.putLong(i, 0x0102030405060708L * (i + 1));
                    bb.putInt(i + 1, 0x7f6e5d4c + i);
                    bb.putShort(i + 2, (short) (0x8001 + i));
                    bb.putChar(i + 3, (char) (0xfffe - i));
                    h = h * 31 + bb.getLong(i) + bb.getInt(i + 1) + bb.getShort(i + 2) + bb.getChar(i + 3);
                }
            }
            System.out.println("bytebuffer " + order + " " + h);
        }
        // Off-heap (null base): stays on the helper / native route.
        long addr = U.allocateMemory(16);
        try {
            U.putLong(addr, 0x0a0b0c0d0e0f1011L);
            long h = 0;
            for (int i = 0; i < REPS; i++) {
                h += U.getInt(null, addr + 1) + U.getShort(null, addr + 3);
            }
            System.out.println("offheap " + h);
        } finally {
            U.freeMemory(addr);
        }
        System.out.println("done");
    }

    // ---------------------------------------------------------------- rows

    static volatile long sink;

    static void row(String name, long n, long t0) {
        long dt = System.nanoTime() - t0;
        System.out.println(name + " " + (dt / n) + " ns/op");
    }

    static void rows(String which) {
        for (String r : which.split(",")) {
            for (int warm = 0; warm < 3; warm++) {
                runRow(r.trim(), warm == 2);
            }
        }
    }

    static void runRow(String r, boolean print) {
        final int n = 20_000_000;
        long t0 = System.nanoTime();
        switch (r) {
            case "heapbb" -> {
                ByteBuffer bb = ByteBuffer.allocate(4096);
                long h = 0;
                for (int i = 0; i < n; i++) {
                    int idx = (i << 2) & 4092;
                    bb.putInt(idx, i);
                    h += bb.getInt(idx);
                }
                sink = h;
            }
            case "unsafeget" -> {
                byte[] b = new byte[4096];
                fill(b);
                long h = 0;
                for (int i = 0; i < n; i++) {
                    h += U.getInt(b, BB + ((i << 2) & 4092));
                }
                sink = h;
            }
            case "unsafeput" -> {
                byte[] b = new byte[4096];
                for (int i = 0; i < n; i++) {
                    U.putInt(b, BB + ((i << 2) & 4092), i);
                }
                sink = b[7];
            }
            case "unaligned" -> {
                ByteBuffer bb = ByteBuffer.allocate(4096).order(ByteOrder.LITTLE_ENDIAN);
                long h = 0;
                for (int i = 0; i < n; i++) {
                    h += bb.getLong(((i * 7) & 4087) | 1);
                }
                sink = h;
            }
            case "fence" -> {
                Object o = new Object();
                long h = 0;
                for (int i = 0; i < n; i++) {
                    h += i;
                    Reference.reachabilityFence(o);
                }
                sink = h;
            }
            default -> {
                System.out.println(r + " unknown");
                return;
            }
        }
        if (print) {
            row(r, n, t0);
        }
    }
}
