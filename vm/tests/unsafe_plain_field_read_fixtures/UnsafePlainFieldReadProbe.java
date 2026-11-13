// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Field;

/**
 * Netty's {@code ReferenceCountUpdater.isLiveNonVolatile} in miniature: a plain
 * {@code Unsafe.getInt(object, fieldOffset)} on a {@code volatile int} field,
 * the read every {@code ByteBuf} access makes through {@code ensureAccessible}.
 * Compiled code must answer it without the native funnel (about 600 ns a read):
 * from the thin helper for a field nothing has CASed, and inline for a field a
 * hardware atomic has published a cell for.
 */
public final class UnsafePlainFieldReadProbe {
    static final sun.misc.Unsafe U;
    static final long OFF_COUNT;
    static final long OFF_WIDE;
    static final long OFF_RATIO;

    static final class Cold {
        volatile int count = 6;
        long wide = 0x0102030405060708L;
        float ratio = 1.5f;
    }

    /** The same shape, whose fields are CASed once so the VM publishes their cells. */
    static final class Hot {
        volatile int count = 6;
        long wide = 0x0102030405060708L;
    }

    static {
        try {
            Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
            f.setAccessible(true);
            U = (sun.misc.Unsafe) f.get(null);
            OFF_COUNT = U.objectFieldOffset(Cold.class.getDeclaredField("count"));
            OFF_WIDE = U.objectFieldOffset(Cold.class.getDeclaredField("wide"));
            OFF_RATIO = U.objectFieldOffset(Cold.class.getDeclaredField("ratio"));
            if (U.objectFieldOffset(Hot.class.getDeclaredField("count")) != OFF_COUNT
                    || U.objectFieldOffset(Hot.class.getDeclaredField("wide")) != OFF_WIDE) {
                throw new IllegalStateException("the two holders must share offsets");
            }
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static long coldBurst(Cold h, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += U.getInt(h, OFF_COUNT);
            sum += U.getLong(h, OFF_WIDE) & 0xFF;
        }
        return sum;
    }

    static long hotBurst(Hot h, int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += U.getInt(h, OFF_COUNT);
            sum += U.getLong(h, OFF_WIDE) & 0xFF;
        }
        return sum;
    }

    public static void main(String[] args) {
        Cold cold = new Cold();
        Hot hot = new Hot();
        // A hardware atomic on each field publishes its cell.
        U.getAndAddInt(hot, OFF_COUNT, 0);
        U.getAndAddLong(hot, OFF_WIDE, 0L);
        long a = 0;
        long b = 0;
        for (int call = 0; call < 1000; call++) {
            a += coldBurst(cold, 2000);
            b += hotBurst(hot, 2000);
        }
        System.out.println("cold=" + a + " hot=" + b);
        cold.count = 9;
        cold.wide = 0x1122334455667788L;
        System.out.println("after=" + U.getInt(cold, OFF_COUNT) + " " + Long.toHexString(U.getLong(cold, OFF_WIDE)));
        // A read of the other kind at a field's offset is the field's bits.
        System.out.println("float=" + (U.getInt(cold, OFF_RATIO) == Float.floatToRawIntBits(1.5f)));
        System.out.println("PLAIN-FIELD-READ-OK");
    }
}
