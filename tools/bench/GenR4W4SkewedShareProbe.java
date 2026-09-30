// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

/**
 * gen r4w4/alloc4 (2026-09-24): MIXED-SIZE allocation on many threads with
 * very different allocation rates — the shape a per-thread TLAB sizer driven
 * by each thread's share of young allocation
 * ({@code CRATONVM_TLAB_SHARE_SIZER}) exists for.
 *
 * <p>Thread {@code t} (0-based) runs {@code base * (t + 1)^2} iterations, so the
 * heaviest of 8 threads allocates 64x the lightest. The lower half of the
 * threads also park for 20 us every 256 iterations (early TLAB retires). Per
 * iteration {@code i}:
 * <ul>
 *   <li>{@code i % 128 == 0}: a {@code long[1024]} (8 KiB — bigger than a
 *       small thread's refill-waste limit, so it exercises the keep-on-miss
 *       arm); adds 3;</li>
 *   <li>else {@code i % 16 == 0}: a {@code byte[1024]}; adds 2;</li>
 *   <li>else: a small {@code Node}; adds 1.</li>
 * </ul>
 * Each fresh array is checked for zero at three positions ({@code nonzero=}
 * must be 0), and a 256-slot ring keeps recent objects alive.
 *
 * <p>Deterministic: per thread {@code sum = n + n/16 + n/128} (n a multiple of
 * 128), so {@code sum = base * 137/128 * T(T+1)(2T+1)/6}. Defaults (8 threads,
 * base 131072): {@code sum=28618752 nonzero=0 ok}.
 * <pre>
 *   java -cp tools/bench GenR4W4SkewedShareProbe
 *   cratonvm -XX:+UseGenerationalGC -Xmx512m --verbose:gc -cp tools/bench GenR4W4SkewedShareProbe
 *   CRATONVM_TLAB_SHARE_SIZER=1 cratonvm -XX:+UseGenerationalGC -Xmx512m --verbose:gc -cp tools/bench GenR4W4SkewedShareProbe
 * </pre>
 * Arguments: threads (default 8), base iterations (default 131072, rounded
 * down to a multiple of 128).
 */
public final class GenR4W4SkewedShareProbe {
    static final class Node {
        final int v;
        Node next;

        Node(int v) {
            this.v = v;
        }
    }

    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        final long base = (args.length > 1 ? Long.parseLong(args[1]) : 131072L) & ~127L;
        final long[] sums = new long[threads];
        final long[] nonzero = new long[threads];
        final CountDownLatch done = new CountDownLatch(threads);
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            final long n = base * (long) (t + 1) * (long) (t + 1);
            final boolean parks = t < threads / 2;
            Thread th = new Thread(() -> {
                Object[] ring = new Object[256];
                long s = 0;
                long nz = 0;
                for (long i = 0; i < n; i++) {
                    int slot = (int) (i & 255);
                    if (i % 128 == 0) {
                        long[] a = new long[1024];
                        int k = (int) (i & 1023);
                        if (a[0] != 0 || a[k] != 0 || a[1023] != 0) {
                            nz++;
                        }
                        a[k] = 3;
                        s += a[k];
                        ring[slot] = a;
                    } else if (i % 16 == 0) {
                        byte[] b = new byte[1024];
                        int k = (int) (i & 1023);
                        if (b[0] != 0 || b[k] != 0 || b[1023] != 0) {
                            nz++;
                        }
                        b[k] = 2;
                        s += b[k];
                        ring[slot] = b;
                    } else {
                        Node node = new Node(1);
                        s += node.v;
                        ring[slot] = node;
                    }
                    if (parks && slot == 255) {
                        LockSupport.parkNanos(20_000L);
                    }
                }
                sums[id] = s;
                nonzero[id] = nz;
                done.countDown();
            }, "skewed-" + t);
            th.start();
        }
        done.await();
        long ms = Math.max(1, (System.nanoTime() - t0) / 1_000_000L);
        long sum = 0;
        long nz = 0;
        long want = 0;
        for (int t = 0; t < threads; t++) {
            sum += sums[t];
            nz += nonzero[t];
            long n = base * (long) (t + 1) * (long) (t + 1);
            want += n + n / 16 + n / 128;
        }
        System.out.println("sum=" + sum + " nonzero=" + nz + " threads=" + threads
                + " base=" + base + " ms=" + ms
                + (sum == want && nz == 0 ? " ok" : " MISMATCH expected=" + want));
    }
}
