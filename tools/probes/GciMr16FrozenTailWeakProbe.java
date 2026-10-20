// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gci i16/mr (2026-10-14): a {@code WeakReference} to a strongly held object
 * must never clear. Page:
 * {@code docs/known-issues/gc/gci-i16mr-g1-frozen-tlab-tail-allocations-read-dead-at-the-remark-20261014.md}.
 *
 * <p>A producer thread allocates a fresh {@code Node} in a tight loop (on
 * CratonVM a compiled loop the stop-the-world take-over may freeze, keeping
 * its TLAB tail across a G1 initial mark); a checker thread wraps the latest
 * node in a {@code WeakReference}, keeps the node itself strongly in a ring
 * beside it, and counts every ring slot whose weak reference no longer
 * answers its node. The main thread runs {@code System.gc()} (a full marking
 * cycle on CratonVM G1: initial mark, remark, full compaction, with no young
 * pause between the first two). A node bumped into a frozen tail after the
 * initial mark read dead at the remark before the gci i16/mr fix, so its
 * weak reference could clear while the ring still held it.
 *
 * <p>Args: {@code [gcs]} (default 40). Stdout (HotSpot, any collector):
 * {@code mr16 tail gcs=N violations=0}. Diagnostics on stderr.
 */
public class GciMr16FrozenTailWeakProbe {
    static final class Node {
        final int seq;

        Node(int seq) {
            this.seq = seq;
        }
    }

    static final int RING = 256;
    static volatile Node latest;
    static volatile boolean run = true;
    static volatile long checks;
    static volatile long violations;
    static volatile long produced;

    public static void main(String[] args) throws Exception {
        int gcs = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        Thread producer = new Thread(() -> {
            int s = 0;
            while (run) {
                latest = new Node(s++);
            }
            produced = s;
        }, "mr16-producer");
        Thread checker = new Thread(() -> {
            Node[] held = new Node[RING];
            @SuppressWarnings("unchecked")
            WeakReference<Node>[] refs = (WeakReference<Node>[]) new WeakReference[RING];
            int i = 0;
            long c = 0;
            long v = 0;
            while (run) {
                Node n = latest;
                if (n != null) {
                    int slot = i++ & (RING - 1);
                    held[slot] = n;
                    refs[slot] = new WeakReference<>(n);
                }
                for (int k = 0; k < RING; k++) {
                    WeakReference<Node> w = refs[k];
                    Node h = held[k];
                    if (w != null && h != null) {
                        c++;
                        if (w.get() != h) {
                            v++;
                        }
                    }
                }
            }
            checks = c;
            violations = v;
        }, "mr16-checker");
        producer.start();
        checker.start();
        Thread.sleep(200);
        for (int g = 0; g < gcs; g++) {
            System.gc();
            Thread.sleep(2);
        }
        run = false;
        producer.join();
        checker.join();
        System.out.println("mr16 tail gcs=" + gcs + " violations=" + violations);
        System.err.println("[mr16] checks=" + checks + " produced=" + produced);
    }
}
