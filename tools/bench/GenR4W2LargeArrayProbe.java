// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;

/**
 * gen r4w2/alloc2 (2026-09-23): N threads allocating arrays larger than a TLAB
 * can serve ({@code > 32 KiB}), so every allocation takes the young slow path
 * {@code GenerationalHeap::try_alloc_young_initialized}. Before wave 2 the
 * whole memset ran with {@code young_from} held; now only the header is
 * written under the lock. The effect is aggregate allocation rate at N = 4, 8
 * against N = 1 (it should scale where it was flat); single-threaded wall must
 * not move.
 *
 * <p>Each allocated array is touched at both ends and checked zero first, so
 * the probe is also a correctness oracle for the outside-the-lock zeroing:
 * {@code nonzero=0} is required.
 * <pre>
 *   java -cp tools/bench GenR4W2LargeArrayProbe 4 20000 262144
 *   cratonvm -XX:+UseGenerationalGC -Xmx2g -cp tools/bench GenR4W2LargeArrayProbe 4 20000 262144
 * </pre>
 * Defaults: 4 threads, 20000 arrays per thread, 262144 bytes each.
 */
public final class GenR4W2LargeArrayProbe {
    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        final int count = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        final int bytes = args.length > 2 ? Integer.parseInt(args[2]) : 262144;
        final long[] nonzero = new long[threads];
        final long[] sums = new long[threads];
        final CountDownLatch done = new CountDownLatch(threads);
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            Thread th = new Thread(() -> {
                long bad = 0;
                long s = 0;
                for (int i = 0; i < count; i++) {
                    byte[] a = new byte[bytes];
                    if (a[0] != 0 || a[bytes / 2] != 0 || a[bytes - 1] != 0) {
                        bad++;
                    }
                    a[0] = 1;
                    a[bytes - 1] = 2;
                    s += a[0] + a[bytes - 1];
                }
                nonzero[id] = bad;
                sums[id] = s;
                done.countDown();
            }, "large-array-" + t);
            th.start();
        }
        done.await();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long bad = 0;
        long sum = 0;
        for (int t = 0; t < threads; t++) {
            bad += nonzero[t];
            sum += sums[t];
        }
        System.out.println("sum=" + sum + " nonzero=" + bad + " threads=" + threads
                + " count=" + count + " bytes=" + bytes + " ms=" + ms);
    }
}
