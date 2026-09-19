// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * One large TENURED reference array, a few element stores per round.
 *
 * This is the shape the per-OBJECT card granularity is worst on, and the only
 * one that separates it from the per-EDGE cost. The write barrier marks the
 * card of the receiver's BASE, and `scan_dirty_cards_inner` then re-reads
 * EVERY reference slot of every object whose header lands in a dirty card. So
 * a single store into element 500,000 of a 1M-element `Object[]` dirties the
 * array's base card and costs a scan of all 1,000,000 elements to rediscover
 * the one edge.
 *
 * Holding the number of STORES fixed and scaling only the array LENGTH is
 * what makes that visible: if `refinement_ms / passes` grows with the length,
 * the scan is O(array length) per dirty card and slot-precise carding is
 * worth building; if it stays flat, it is not.
 *
 * Usage: OldGenWideArrayCardProbe [length] [rounds] [churnDepth] [writes]
 *   length      elements in the retained reference array (default 262144)
 *   rounds      churn iterations (default 700)
 *   churnDepth  depth of each discarded tree (default 16)
 *   writes      elements stored per round (default 4)
 *
 * The checksums are pure functions of the arguments.
 */
public final class OldGenWideArrayCardProbe {
    static final class Node {
        Node a, b;
        int v;
        Node(int v) { this.v = v; }
    }

    static Node build(int depth, int v) {
        Node n = new Node(v);
        if (depth > 0) {
            n.a = build(depth - 1, v * 2);
            n.b = build(depth - 1, v * 2 + 1);
        }
        return n;
    }

    public static void main(String[] args) {
        int length = args.length > 0 ? Integer.parseInt(args[0]) : 262144;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 700;
        int churnDepth = args.length > 2 ? Integer.parseInt(args[2]) : 16;
        int writes = args.length > 3 ? Integer.parseInt(args[3]) : 4;

        // The retained array. Filled with a shared immutable so the array is
        // dense (every slot a real reference the scan must read) without the
        // fill itself being the live set.
        Object filler = new Object();
        Object[] wide = new Object[length];
        for (int i = 0; i < length; i++) {
            wide[i] = filler;
        }

        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            // The old-to-young edges: `writes` fresh young objects stored into
            // the tenured array, spread across it so no two share a card.
            for (int i = 0; i < writes; i++) {
                int idx = (int) (((long) (r * writes + i) * 7919L) % length);
                wide[idx] = new int[] { r, i };
            }
            Node garbage = build(churnDepth, r);
            acc += garbage.v;
            // Read them back: a collector that lost an edge is caught here
            // rather than reported as a faster arm.
            for (int i = 0; i < writes; i++) {
                int idx = (int) (((long) (r * writes + i) * 7919L) % length);
                Object held = wide[idx];
                if (!(held instanceof int[])) {
                    throw new IllegalStateException(
                            "lost the old->young edge at round " + r + " index " + idx);
                }
                int[] pair = (int[]) held;
                acc += pair[0] + pair[1];
            }
        }

        int live = 0;
        for (int i = 0; i < length; i++) {
            if (wide[i] != filler) {
                live++;
            }
        }
        System.out.println("length=" + length + " rounds=" + rounds
                + " writes=" + writes + " churn=" + acc + " mutated=" + live);
        System.out.println("PASS OldGenWideArrayCardProbe");
    }
}
