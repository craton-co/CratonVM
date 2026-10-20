// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import io.netty.buffer.ByteBuf;
import io.netty.buffer.ByteBufAllocator;
import io.netty.buffer.UnpooledByteBufAllocator;

/**
 * gci i5g/g1r: the reproduction of
 * {@code gc-g1-netty-direct-buffer-methodhandle-result-is-a-reclaimed-object-20261010}
 * as a probe. Needs netty-common and netty-buffer 4.2.16.Final on the class
 * path (compile with {@code -cp netty-common.jar;netty-buffer.jar}; for the
 * battery, unpack both jars into the class dir).
 *
 * <p>{@code GciG1rNettyDirectProbe <adaptive|unpooled> <size|lo-hi> <iters>}:
 * the page's loop (a daemon churn thread; per iteration a direct buffer of the
 * size, {@code writeInt} / {@code readInt}, an {@code nioBuffer} view, release).
 * stdout: {@code ok sum=<n>}, or the first exception's class and message
 * (deterministic on HotSpot, which prints {@code ok}).
 */
public class GciG1rNettyDirectProbe {
    static volatile Object sink;

    public static void main(String[] a) {
        String mode = a.length > 0 ? a[0] : "adaptive";
        String spec = a.length > 1 ? a[1] : "70000";
        int iters = a.length > 2 ? Integer.parseInt(a[2]) : 20000;
        int lo;
        int hi;
        int dash = spec.indexOf('-');
        if (dash > 0) {
            lo = Integer.parseInt(spec.substring(0, dash));
            hi = Integer.parseInt(spec.substring(dash + 1));
        } else {
            lo = Integer.parseInt(spec);
            hi = lo;
        }
        ByteBufAllocator alloc = mode.equals("unpooled")
                ? UnpooledByteBufAllocator.DEFAULT
                : ByteBufAllocator.DEFAULT;
        Thread t = new Thread(() -> {
            Object[] ring = new Object[64];
            int i = 0;
            while (true) {
                ring[i++ & 63] = new byte[(i & 7) * 1024 + 16];
            }
        });
        t.setDaemon(true);
        t.start();
        long t0 = System.nanoTime();
        long sum = 0;
        try {
            for (int i = 0; i < iters; i++) {
                int size = lo + (hi > lo ? i % (hi - lo + 1) : 0);
                ByteBuf b = alloc.directBuffer(size);
                b.writeInt(i);
                sum += b.readInt();
                sum += b.nioBuffer(0, 4).capacity();
                b.release();
                if ((i & 63) == 0) {
                    sink = new byte[8192];
                }
            }
            System.out.println("ok sum=" + sum);
        } catch (Throwable e) {
            System.out.println(e.getClass().getName() + ": " + e.getMessage());
            e.printStackTrace();
        }
        System.err.println("[g1r-netty] mode=" + mode + " size=" + spec + " wall_ms="
                + (System.nanoTime() - t0) / 1_000_000);
    }
}
