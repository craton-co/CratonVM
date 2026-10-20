// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicLongArray;

/**
 * nio21 wave 4, lane L4: the inline {@code Unsafe.{get,put}{Int,Long}Volatile} prefix on
 * primitive-array elements (page
 * {@code r21-unsafe-volatile-accessors-are-thin-calls-not-inline-and-getandadd-is-not-one-xadd-FIXED-20261012.md}).
 * Under {@code --jdk-only} these are what {@code AtomicIntegerArray} / {@code AtomicLongArray} and
 * an array element {@code VarHandle} reach for {@code get} / {@code set} / {@code getVolatile} /
 * {@code setVolatile}. A byte[] view {@code VarHandle} row (plain modes) is the unaligned-path control.
 *
 * <p>Conformance (default): every row prints values only, and each row runs hot enough to compile.
 * Bench ({@code -Dnio21.rows=bench}): {@code <row> <ns> ns/op} lines.
 *
 * <pre>
 *   cratonvm --jdk-only   -cp tools/probes Nio21VolatileArray
 *   cratonvm --compatible -cp tools/probes Nio21VolatileArray
 *   CRATONVM_JIT_UNSAFE_VOLATILE_INLINE=0 cratonvm --jdk-only -cp tools/probes Nio21VolatileArray
 *   cratonvm --jdk-only -cp tools/probes -Dnio21.rows=bench Nio21VolatileArray
 *   CRATONVM_JIT_UNSAFE_RMW_INLINE=0 cratonvm --jdk-only -cp tools/probes Nio21VolatileArray
 * </pre>
 *
 * <p>nio21 wave 5, lane L3: the {@code rmw} row drives the inline array CAS / getAndAdd /
 * getAndSet ({@code LOCK CMPXCHG} / {@code LOCK XADD} / {@code XCHG}) through
 * {@code AtomicIntegerArray}, {@code AtomicLongArray} and array {@code VarHandle}s; its bench row
 * is {@code rmw}.
 */
public final class Nio21VolatileArray {
    static final VarHandle INTS = MethodHandles.arrayElementVarHandle(int[].class);
    static final VarHandle LONGS = MethodHandles.arrayElementVarHandle(long[].class);
    static final VarHandle VIEW_INT_LE =
            MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
    static final VarHandle VIEW_LONG_LE =
            MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN);
    static final int ITERS = Integer.getInteger("nio21.iters", 200_000);
    static volatile long sink;

    static long aiaRow(AtomicIntegerArray a) {
        long acc = 0;
        for (int i = 0; i < ITERS; i++) {
            int k = i & 15;
            a.set(k, a.get(k) + 3);
            a.lazySet((k + 1) & 15, i);
            acc += a.get((k + 7) & 15);
        }
        return acc;
    }

    static long alaRow(AtomicLongArray a) {
        long acc = 0;
        for (int i = 0; i < ITERS; i++) {
            int k = i & 15;
            a.set(k, a.get(k) + 0x1_0000_0001L);
            acc ^= a.get((k + 5) & 15) * 31;
        }
        return acc;
    }

    static long vhRow(int[] ia, long[] la) {
        long acc = 0;
        for (int i = 0; i < ITERS; i++) {
            int k = i & 7;
            INTS.setVolatile(ia, k, (int) INTS.getVolatile(ia, k) - 1);
            LONGS.setVolatile(la, k, (long) LONGS.getVolatile(la, k) + i);
            acc += (int) INTS.getVolatile(ia, (k + 3) & 7) + (long) LONGS.getVolatile(la, (k + 1) & 7);
        }
        return acc;
    }

    /** A byte[] view handle: plain access only on JDK 25 (volatile modes throw). */
    static long viewRow(byte[] b) {
        long acc = 0;
        for (int i = 0; i < ITERS; i++) {
            int off = (i & 3) * 8;
            VIEW_INT_LE.set(b, off, (int) VIEW_INT_LE.get(b, off) + 1);
            VIEW_LONG_LE.set(b, off, (long) VIEW_LONG_LE.get(b, off) ^ i);
            acc += (int) VIEW_INT_LE.get(b, off + 4);
        }
        return acc;
    }

    /** nio21 wave 5 L3: the array RMW forms (compareAndSet{Int,Long}, getAndAdd*, getAndSet*). */
    static long rmwRow(AtomicIntegerArray a, AtomicLongArray l, int[] ia, long[] la) {
        long acc = 0;
        for (int i = 0; i < ITERS; i++) {
            int k = i & 15;
            acc += a.getAndAdd(k, 3);
            acc += a.compareAndSet((k + 1) & 15, a.get((k + 1) & 15), i) ? 1 : 0;
            acc += a.getAndSet((k + 2) & 15, -i);
            acc ^= l.incrementAndGet(k & 7);
            acc += l.compareAndSet((k + 3) & 7, i, 5L) ? 2 : 0;
            acc += (int) INTS.getAndAdd(ia, k & 7, -1);
            acc += (long) LONGS.getAndSet(la, k & 7, (long) i << 20);
            acc += INTS.compareAndSet(ia, (k + 5) & 7, i, 0) ? 4 : 0;
        }
        return acc;
    }

    static String bytes(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) {
            sb.append(Integer.toHexString(x & 0xFF)).append(' ');
        }
        return sb.toString().trim();
    }

    static void conformance() {
        AtomicIntegerArray aia = new AtomicIntegerArray(16);
        long r1 = 0;
        for (int rep = 0; rep < 3; rep++) {
            r1 += aiaRow(aia);
        }
        System.out.println("aia " + r1 + " " + aia);
        AtomicLongArray ala = new AtomicLongArray(16);
        long r2 = 0;
        for (int rep = 0; rep < 3; rep++) {
            r2 ^= alaRow(ala);
        }
        System.out.println("ala " + r2 + " " + ala);
        int[] ia = {1, 2, 3, 4, 5, 6, 7, 8};
        long[] la = {9, 8, 7, 6, 5, 4, 3, 2};
        long r3 = 0;
        for (int rep = 0; rep < 3; rep++) {
            r3 += vhRow(ia, la);
        }
        System.out.println("vh " + r3 + " " + java.util.Arrays.toString(ia) + " "
                + java.util.Arrays.toString(la));
        byte[] b = new byte[40];
        long r4 = 0;
        for (int rep = 0; rep < 3; rep++) {
            r4 += viewRow(b);
        }
        System.out.println("view " + r4 + " " + bytes(b));
        AtomicIntegerArray ra = new AtomicIntegerArray(16);
        AtomicLongArray rl = new AtomicLongArray(8);
        int[] ri = new int[8];
        long[] rla = new long[8];
        long r5 = 0;
        for (int rep = 0; rep < 3; rep++) {
            r5 = r5 * 31 + rmwRow(ra, rl, ri, rla);
        }
        System.out.println("rmw " + r5 + " " + ra + " " + rl + " " + java.util.Arrays.toString(ri) + " "
                + java.util.Arrays.toString(rla));
        // A byte[] view refuses the volatile modes before it reaches Unsafe.
        try {
            VIEW_INT_LE.getVolatile(b, 0);
            System.out.println("view-volatile no-throw");
        } catch (RuntimeException e) {
            System.out.println("view-volatile " + e.getClass().getName());
        }
        // Out-of-bounds index: the VarHandle's own check, before Unsafe.
        try {
            INTS.setVolatile(ia, 8, 1);
            System.out.println("oob no-throw");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("oob " + e.getMessage());
        }
    }

    static void bench() {
        AtomicIntegerArray aia = new AtomicIntegerArray(16);
        AtomicLongArray ala = new AtomicLongArray(16);
        int[] ia = new int[8];
        long[] la = new long[8];
        byte[] b = new byte[40];
        AtomicIntegerArray ra = new AtomicIntegerArray(16);
        AtomicLongArray rl = new AtomicLongArray(8);
        for (int w = 0; w < 10; w++) {
            sink += aiaRow(aia) + alaRow(ala) + vhRow(ia, la) + viewRow(b) + rmwRow(ra, rl, ia, la);
        }
        String[] names = {"aia", "ala", "vh", "view", "rmw"};
        for (int r = 0; r < names.length; r++) {
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (r) {
                    case 0: sink += aiaRow(aia); break;
                    case 1: sink += alaRow(ala); break;
                    case 2: sink += vhRow(ia, la); break;
                    case 3: sink += viewRow(b); break;
                    default: sink += rmwRow(ra, rl, ia, la); break;
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(names[r] + " " + (best / ITERS) + " ns/op");
        }
    }

    public static void main(String[] args) {
        if ("bench".equals(System.getProperty("nio21.rows"))) {
            bench();
        } else {
            conformance();
        }
    }
}
