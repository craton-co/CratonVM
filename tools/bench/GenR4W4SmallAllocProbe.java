// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;

/**
 * gen r4w4/alloc4 (2026-09-24): the SMALL-OBJECT allocation rate — the shape
 * the allocation fast path, the TLAB refill and its zeroing are all on.
 *
 * <p>Each of {@code threads} workers runs {@code iters} iterations of: one
 * {@code new Node} (a short chain, cut every 64 nodes so it dies young) and one
 * {@code new int[i & 7]}. Each iteration checks the fresh array reads zero —
 * the zero-once skip ({@code CRATONVM_GEN_ZERO_ONCE}, default ON) must not
 * hand out a non-zero byte — and adds {@code node.v + (chain cut ? 1 : 0) +
 * array.length} to its sum.
 *
 * <p>Deterministic output, independent of timing and collector:
 * per thread {@code sum = n(n-1)/2 + ceil(n/64) + 28 * n/8} for n = iters
 * (n a multiple of 8), times {@code threads}. Defaults (1 thread, 50,000,000
 * iterations): {@code sum=1250000150781250 nonzero=0 ok}.
 * <pre>
 *   java -cp tools/bench GenR4W4SmallAllocProbe
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench GenR4W4SmallAllocProbe
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench GenR4W4SmallAllocProbe 4 50000000
 * </pre>
 * Arguments: threads (default 1), iterations per thread (default 50,000,000,
 * rounded down to a multiple of 8).
 */
public final class GenR4W4SmallAllocProbe {
    static final class Node {
        final long v;
        final Node next;

        Node(long v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    static long expected(long n) {
        return n * (n - 1) / 2 + (n + 63) / 64 + 28L * (n / 8);
    }

    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        final long iters = (args.length > 1 ? Long.parseLong(args[1]) : 50_000_000L) & ~7L;
        final long[] sums = new long[threads];
        final long[] nonzero = new long[threads];
        final CountDownLatch done = new CountDownLatch(threads);
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            Thread th = new Thread(() -> {
                long s = 0;
                long nz = 0;
                Node node = null;
                for (long i = 0; i < iters; i++) {
                    node = new Node(i, (i % 64 == 0) ? null : node);
                    int[] a = new int[(int) (i & 7)];
                    for (int k = 0; k < a.length; k++) {
                        if (a[k] != 0) {
                            nz++;
                        }
                    }
                    s += node.v + (node.next == null ? 1 : 0) + a.length;
                }
                sums[id] = s;
                nonzero[id] = nz;
                done.countDown();
            }, "small-alloc-" + t);
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
        long want = expected(iters) * threads;
        long objects = 2L * iters * threads;
        System.out.println("sum=" + sum + " nonzero=" + nz + " threads=" + threads
                + " iters=" + iters + " ms=" + ms + " mallocs_per_s=" + (objects * 1000L / ms)
                + (sum == want && nz == 0 ? " ok" : " MISMATCH expected=" + want));
    }
}
