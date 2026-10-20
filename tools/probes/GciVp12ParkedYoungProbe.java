// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.locks.LockSupport;

/**
 * gci i12/vp: parked virtual threads across many YOUNG collections.
 *
 *   young N R [STRIDE]
 *
 * N virtual threads park (unmounted) holding small object graphs in the
 * locals of a recursion of known depth. The main thread then runs R rounds of
 * allocation (young collections, no System.gc() except one full collection
 * at round R/2). Before each round's allocation it wakes every thread whose
 * index is congruent to the round modulo STRIDE (default 10; 0 = never): the
 * woken thread checks its graph, builds a NEW (young) one in the same local
 * and parks again -- so parked words are both long-lived (promoted while
 * parked) and fresh. At the end every thread is released and re-checks every
 * frame's graph.
 *
 * stdout is a pure function of the arguments (no timing, no ids); timing and
 * diagnostics go to stderr ("[vp12] ...").
 */
public class GciVp12ParkedYoungProbe {
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static final AtomicInteger REBUILT = new AtomicInteger();
    static volatile Object sink;
    static volatile boolean release;
    static AtomicIntegerArray generation;

    static final class Node {
        final int id;
        final long[] payload;
        Node next;
        Object side;

        Node(int id, int len) {
            this.id = id;
            this.payload = new long[len];
            for (int i = 0; i < len; i++) {
                payload[i] = (long) id * 31L + i;
            }
        }
    }

    static Node build(int seed, int n) {
        Node head = null;
        for (int i = n - 1; i >= 0; i--) {
            Node x = new Node(seed * 1000 + i, 2 + (i % 5));
            x.next = head;
            if (i % 2 == 0) {
                x.side = "p" + seed + "_" + i;
            } else {
                x.side = new int[] { seed, i };
            }
            head = x;
        }
        return head;
    }

    static long sum(Node h) {
        long s = 0;
        for (Node x = h; x != null; x = x.next) {
            s = s * 1_000_003L + x.id;
            for (long v : x.payload) {
                s += v;
            }
            Object side = x.side;
            if (side instanceof String) {
                s += ((String) side).hashCode();
            } else if (side instanceof int[]) {
                int[] a = (int[]) side;
                s += a[0] * 7L + a[1];
            } else {
                s += 999_999_937L;
            }
        }
        return s;
    }

    static void garbage(int k) {
        for (int i = 0; i < k; i++) {
            sink = new byte[128 + (i * 37 % 768)];
        }
    }

    static long body(int idx, int depth, CountDownLatch parkedOnce) {
        Node mine = build(idx, 4);
        long e = sum(mine);
        long acc;
        if (depth > 0) {
            acc = body(idx, depth - 1, parkedOnce);
        } else {
            acc = parkPoint(idx, parkedOnce);
        }
        if (sum(mine) != e) {
            MISMATCH.incrementAndGet();
            System.err.println("[vp12] frame mismatch idx=" + idx + " depth=" + depth);
        }
        return acc * 13L + e;
    }

    static long parkPoint(int idx, CountDownLatch parkedOnce) {
        Node a = build(idx + 100_000, 6);
        long ea = sum(a);
        long acc = ea;
        int seen = 0;
        parkedOnce.countDown();
        while (!release) {
            LockSupport.park();
            if (sum(a) != ea) {
                MISMATCH.incrementAndGet();
                System.err.println("[vp12] parkPoint mismatch idx=" + idx + " gen=" + seen);
            }
            int g = generation.get(idx);
            if (g != seen) {
                seen = g;
                a = build(idx * 7 + g * 100_003, 6);
                ea = sum(a);
                acc = acc * 31L + ea;
                REBUILT.incrementAndGet();
            }
        }
        if (sum(a) != ea) {
            MISMATCH.incrementAndGet();
            System.err.println("[vp12] final parkPoint mismatch idx=" + idx);
        }
        return acc;
    }

    static void young(int n, int rounds, int stride) throws Exception {
        generation = new AtomicIntegerArray(n);
        CountDownLatch parkedOnce = new CountDownLatch(n);
        long[] results = new long[n];
        List<Thread> ts = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            final int idx = i;
            ts.add(Thread.ofVirtual().name("vp12-" + i).start(() -> {
                results[idx] = body(idx, idx % 3, parkedOnce);
            }));
        }
        parkedOnce.await();
        int expectRebuilt = 0;
        long allocNanos = 0;
        for (int r = 0; r < rounds; r++) {
            if (stride > 0) {
                for (int i = r % stride; i < n; i += stride) {
                    generation.set(i, r + 1);
                    expectRebuilt++;
                    LockSupport.unpark(ts.get(i));
                }
                // Each woken thread rebuilds exactly once per generation.
                for (int spin = 0; REBUILT.get() < expectRebuilt && spin < 600_000; spin++) {
                    Thread.sleep(1);
                }
            }
            if (r == rounds / 2) {
                System.gc();
            }
            long t0 = System.nanoTime();
            for (int k = 0; k < 8; k++) {
                garbage(4000);
            }
            allocNanos += System.nanoTime() - t0;
        }
        release = true;
        for (Thread t : ts) {
            LockSupport.unpark(t);
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        for (long r : results) {
            total = total * 1_000_003L + r;
        }
        System.out.println("vp12 young threads=" + n + " rounds=" + rounds + " stride=" + stride
                + " rebuilt=" + REBUILT.get() + "/" + expectRebuilt
                + " mismatches=" + MISMATCH.get() + " checksum=" + total);
        System.err.println("[vp12] alloc_ms=" + (allocNanos / 1_000_000) + " threads=" + n
                + " rounds=" + rounds);
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "young";
        if (!"young".equals(mode)) {
            throw new IllegalArgumentException("mode: young N R [STRIDE]");
        }
        young(args.length > 1 ? Integer.parseInt(args[1]) : 1000,
                args.length > 2 ? Integer.parseInt(args[2]) : 40,
                args.length > 3 ? Integer.parseInt(args[3]) : 10);
    }
}
