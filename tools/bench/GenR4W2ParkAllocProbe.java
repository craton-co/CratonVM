// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.locks.LockSupport;

/**
 * gen r4w2/alloc2 (2026-09-23): the PARK-HEAVY allocation shape the TLAB
 * sizer pages keep asking for and the tree did not have.
 *
 * <p>{@code threads} workers each run {@code rounds} rounds of: allocate
 * {@code perRound} small objects (a few KiB — a fraction of one 256 KiB TLAB),
 * then park for ~50 us. Every park is an early TLAB retire, so this is the
 * population {@code CRATONVM_TLAB_SIZE_RETIRED} changes and the one
 * {@code BinT} (a drain workload) cannot show. Read
 * {@code [GC] tlab-waste: retires= wasteful_retires= carved= unused=} and
 * {@code [GC] generational: minor=} under {@code --verbose:gc}.
 *
 * <p>Deterministic output: {@code sum=} depends only on the arguments.
 * <pre>
 *   java -cp tools/bench GenR4W2ParkAllocProbe 8 20000 64
 *   cratonvm -XX:+UseGenerationalGC -Xmx512m --verbose:gc -cp tools/bench GenR4W2ParkAllocProbe 8 20000 64
 * </pre>
 * Defaults: 8 threads, 20000 rounds, 64 objects per round.
 */
public final class GenR4W2ParkAllocProbe {
    static final class Node {
        final int v;
        Node next;

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    public static void main(String[] args) throws Exception {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        final int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        final int perRound = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        final long[] sums = new long[threads];
        final CountDownLatch done = new CountDownLatch(threads);
        long t0 = System.nanoTime();
        for (int t = 0; t < threads; t++) {
            final int id = t;
            Thread th = new Thread(() -> {
                long s = 0;
                for (int r = 0; r < rounds; r++) {
                    Node head = null;
                    for (int i = 0; i < perRound; i++) {
                        head = new Node(i + r, head);
                    }
                    for (Node n = head; n != null; n = n.next) {
                        s += n.v;
                    }
                    LockSupport.parkNanos(50_000L);
                }
                sums[id] = s;
                done.countDown();
            }, "park-alloc-" + t);
            th.start();
        }
        done.await();
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long sum = 0;
        for (long s : sums) {
            sum += s;
        }
        System.out.println("sum=" + sum + " threads=" + threads + " rounds=" + rounds
                + " perRound=" + perRound + " ms=" + ms);
    }
}
