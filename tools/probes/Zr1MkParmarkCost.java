// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z1, lane mk (2026-10-12).
//
// The deciding probe for two default flips on ZGC marking:
//   * CRATONVM_ZGC_PARMARK=auto (parallel stop-the-world mark) vs the serial
//     loop, on a large live set -- read `mark=` and `mark_us=` off
//     `[GC] zgc-real:` / `[GC] zgc-pause:` (--verbose:gc);
//   * CRATONVM_ZGC_CONC_START=auto (concurrent mark) vs stop-the-world, with
//     mutator threads storing references while the cycle runs -- read
//     `[GC] zgc-markend:` scanned_at_safepoint / scanned_concurrently and the
//     window fit.
//
// Shape: a live complete binary tree of 2^depth - 1 nodes (BigLive-like), a
// churn loop that allocates short-lived garbage so the collector triggers on
// its own, and `mutators` threads that keep swapping random subtrees of the
// live tree (reference stores into old objects: the SATB barrier's load).
// The tree's node count and a structural checksum are invariant under the
// swaps, so every arm must print the same line as HotSpot.
//
// Output:
//   ZR1MK-PARMARK depth=<d> nodes=<2^d-1> checksum=<c> ok
//   (stderr) ZR1MK-PARMARK gc_count=<n> gc_ms=<t> wall_ms=<w> swaps=<s>
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.util.concurrent.ThreadLocalRandom;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class Zr1MkParmarkCost {
    static final class Node {
        Node left, right;
        final int depth;
        Node(int depth) { this.depth = depth; }
    }

    static Node build(int d) {
        Node n = new Node(d);
        if (d > 1) {
            n.left = build(d - 1);
            n.right = build(d - 1);
        }
        return n;
    }

    static long count(Node n) {
        long c = 0;
        java.util.ArrayDeque<Node> st = new java.util.ArrayDeque<>();
        st.push(n);
        while (!st.isEmpty()) {
            Node x = st.pop();
            c++;
            if (x.left != null) st.push(x.left);
            if (x.right != null) st.push(x.right);
        }
        return c;
    }

    /** Sum of depth over all nodes: invariant under swapping same-depth subtrees. */
    static long checksum(Node n) {
        long c = 0;
        java.util.ArrayDeque<Node> st = new java.util.ArrayDeque<>();
        st.push(n);
        while (!st.isEmpty()) {
            Node x = st.pop();
            c += x.depth;
            if (x.left != null) st.push(x.left);
            if (x.right != null) st.push(x.right);
        }
        return c;
    }

    public static void main(String[] args) throws Exception {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 21;
        int seconds = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        int mutators = args.length > 2 ? Integer.parseInt(args[2]) : 0;
        long t0 = System.nanoTime();
        final Node root = build(depth);
        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong swaps = new AtomicLong();
        Thread[] ts = new Thread[mutators];
        for (int m = 0; m < mutators; m++) {
            ts[m] = new Thread(() -> {
                ThreadLocalRandom rnd = ThreadLocalRandom.current();
                while (!stop.get()) {
                    // Walk to a random interior node and swap its children's
                    // left subtrees: same depth, so the checksum is invariant.
                    Node a = root;
                    int steps = rnd.nextInt(1, Math.max(2, depth - 3));
                    for (int i = 0; i < steps && a.left != null; i++) a = rnd.nextBoolean() ? a.left : a.right;
                    Node l = a.left, r = a.right;
                    if (l == null || r == null || l.left == null || r.left == null) continue;
                    synchronized (a) {
                        Node tmp = l.left;
                        l.left = r.left;
                        r.left = tmp;
                    }
                    swaps.incrementAndGet();
                }
            });
            ts[m].setDaemon(true);
            ts[m].start();
        }
        long end = System.nanoTime() + seconds * 1_000_000_000L;
        Object[] junk = new Object[4096];
        long i = 0;
        while (System.nanoTime() < end) {
            junk[(int) (i++ & 4095)] = new Node(0);
            if ((i & 0xFFFF) == 0) junk[(int) (i & 4095)] = new byte[16 * 1024];
        }
        stop.set(true);
        for (Thread t : ts) t.join();
        long nodes = count(root);
        long sum = checksum(root);
        long wantNodes = (1L << depth) - 1;
        long wantSum = 0;
        for (int d = 1; d <= depth; d++) wantSum += (long) d * (1L << (depth - d));
        long gcCount = 0, gcMs = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            gcCount += Math.max(0, b.getCollectionCount());
            gcMs += Math.max(0, b.getCollectionTime());
        }
        boolean ok = nodes == wantNodes && sum == wantSum;
        System.out.println("ZR1MK-PARMARK depth=" + depth + " nodes=" + nodes + " checksum=" + sum
                + (ok ? " ok" : " BAD"));
        System.err.println("ZR1MK-PARMARK gc_count=" + gcCount + " gc_ms=" + gcMs + " wall_ms="
                + (System.nanoTime() - t0) / 1_000_000 + " swaps=" + swaps.get());
    }
}
