// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;

/**
 * gen r4w4/alloc4 (2026-09-24): HUMONGOUS array allocation on concurrent
 * threads — the shape whose body memset used to run with the old-generation
 * lock held, serialising every other thread's humongous allocation, old-gen
 * spill and concurrent-sweep slice behind it
 * ({@code CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED}, default ON, zeroes a
 * PRIMITIVE body after the lock drops).
 *
 * <p>Each of {@code threads} workers allocates {@code iters} arrays of
 * {@code bytes} bytes: three {@code long[]} (primitive — the unlocked path) to
 * every one {@code Object[]} (reference — keeps the locked memset). Every
 * fresh array is checked for zero (null) at its first, middle and last
 * element, then written at those three places so a block reused dirty would
 * show. The last two arrays per thread stay reachable.
 *
 * <p>Deterministic: {@code sum = threads * iters * (bytes / 8)} (the sum of
 * element counts — both array kinds have {@code bytes / 8} elements) and
 * {@code nonzero=0}. Defaults (4 threads, 200 iterations, 24 MiB):
 * {@code sum=2516582400 nonzero=0 ok}. With {@code -Xmn64m} each semi is
 * 32 MiB, so the humongous threshold is 16 MiB: every {@code long[]} here is
 * humongous, and so is every {@code Object[]} unless references are 4 bytes
 * (then it is 12 MiB and young — still zero-checked, just not this path).
 * <pre>
 *   java -Xmx2g -cp tools/bench GenR4W4HumongousProbe
 *   cratonvm -XX:+UseGenerationalGC -Xmx2g -Xmn64m --verbose:gc -cp tools/bench GenR4W4HumongousProbe 4 200 25165824
 *   CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED=0 cratonvm -XX:+UseGenerationalGC -Xmx2g -Xmn64m --verbose:gc -cp tools/bench GenR4W4HumongousProbe 4 200 25165824
 * </pre>
 * Arguments: threads (default 4), iterations per thread (default 200), array
 * bytes (default 25165824 = 24 MiB, rounded down to a multiple of 8).
 */
public final class GenR4W4HumongousProbe {
    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        final int iters = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        final int bytes = (args.length > 2 ? Integer.parseInt(args[2]) : 25165824) & ~7;
        final int len = bytes / 8;
        final long[] sums = new long[threads];
        final long[] nonzero = new long[threads];
        final CountDownLatch done = new CountDownLatch(threads);
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            Thread th = new Thread(() -> {
                Object keep0 = null;
                Object keep1 = null;
                long s = 0;
                long nz = 0;
                int mid = len / 2;
                int last = len - 1;
                for (int i = 0; i < iters; i++) {
                    Object fresh;
                    if (i % 4 == 3) {
                        Object[] r = new Object[len];
                        if (r[0] != null || r[mid] != null || r[last] != null) {
                            nz++;
                        }
                        r[0] = r;
                        r[mid] = r;
                        r[last] = r;
                        s += r.length;
                        fresh = r;
                    } else {
                        long[] a = new long[len];
                        if (a[0] != 0 || a[mid] != 0 || a[last] != 0) {
                            nz++;
                        }
                        a[0] = i + 1;
                        a[mid] = i + 1;
                        a[last] = i + 1;
                        s += a.length;
                        fresh = a;
                    }
                    keep0 = keep1;
                    keep1 = fresh;
                }
                if (keep0 == keep1 && iters > 1) {
                    nz++;
                }
                sums[id] = s;
                nonzero[id] = nz;
                done.countDown();
            }, "humongous-" + t);
            th.start();
        }
        done.await();
        long ms = Math.max(1, (System.nanoTime() - t0) / 1_000_000L);
        long sum = 0;
        long nz = 0;
        for (int t = 0; t < threads; t++) {
            sum += sums[t];
            nz += nonzero[t];
        }
        long want = (long) threads * iters * len;
        long mib = (long) threads * iters * bytes / (1024L * 1024L);
        System.out.println("sum=" + sum + " nonzero=" + nz + " threads=" + threads
                + " iters=" + iters + " bytes=" + bytes + " ms=" + ms
                + " mib_per_s=" + (mib * 1000L / ms)
                + (sum == want && nz == 0 ? " ok" : " MISMATCH expected=" + want));
    }
}
