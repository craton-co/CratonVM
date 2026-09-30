// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * A large tenured set with a HANDFUL of old-to-young edges per collection.
 *
 * This is the shape `OldGenRsetProbe` and `OldToYoungEdgeProbe` between them
 * cannot produce, and it is the only one on which the dirty-card scan's
 * PREFIX WALK is visible:
 *
 *   * `OldGenRsetProbe` retains a large tree and never stores into it, so the
 *     dirty set is EMPTY and `scan_dirty_cards_inner` returns before walking
 *     anything at all.
 *   * `OldToYoungEdgeProbe` stores into every retained node, so the scan's
 *     cost is dominated by the edges it legitimately delivers (measured at
 *     34-50 ns per edge, flat across a 3.5x growth of the old generation).
 *
 * `OldGen::walk_objects_in_card_ranges` reconstructs object boundaries by
 * striding header-to-header from the start of each allocated region, because
 * the old generation has no object-start index. So a single dirty card near
 * the END of a large old generation costs a walk over every object before it,
 * while delivering one edge. Holding the number of dirty cards FIXED and
 * scaling only the retained set is what separates that O(old objects) term
 * from the O(edges) one; if `refinement_ms / passes` grows with the retained
 * size here, the prefix walk is real, and if it stays flat it is not.
 *
 * Usage: OldGenSparseCardProbe [retainedDepth] [rounds] [churnDepth] [writes]
 *   retainedDepth  binary tree held for the whole run (default 18)
 *   rounds         churn iterations (default 200)
 *   churnDepth     depth of each discarded tree (default 14)
 *   writes         retained nodes mutated per round (default 1)
 *
 * The mutated nodes are chosen at the DEEP end of the retained tree, so their
 * cards sit late in the old generation and the prefix a scan must walk to
 * reach them is as long as this shape allows.
 *
 * Both checksums are pure functions of the arguments, so any arm that prints
 * different numbers did different work.
 */
public final class OldGenSparseCardProbe {
    static final class Node {
        Node a, b;
        Object mutable;
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

    static long sum(Node n) {
        return n == null ? 0 : n.v + sum(n.a) + sum(n.b);
    }

    /** The right spine: the deepest, latest-allocated nodes of the tree. */
    static Node spine(Node n, int down) {
        for (int i = 0; i < down && n.b != null; i++) {
            n = n.b;
        }
        return n;
    }

    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 18;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int churnDepth = args.length > 2 ? Integer.parseInt(args[2]) : 14;
        int writes = args.length > 3 ? Integer.parseInt(args[3]) : 1;

        Node retained = build(depth, 1);
        long checksum = sum(retained);

        // The write targets: `writes` distinct nodes down the right spine, so
        // the dirty cards are few, fixed in number, and late in the old gen.
        Node[] targets = new Node[writes];
        for (int i = 0; i < writes; i++) {
            targets[i] = spine(retained, depth - (i % Math.max(1, depth)));
        }

        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            // The old-to-young edges: a fresh young object stored into a
            // tenured field. Exactly `writes` dirty cards per round.
            for (int i = 0; i < writes; i++) {
                targets[i].mutable = new int[] { r, i };
            }
            // Young garbage, to make the collections happen.
            Node garbage = build(churnDepth, r);
            acc += garbage.v;
            // Read every edge back, so a collector that lost one is caught
            // here rather than reported as a faster arm.
            for (int i = 0; i < writes; i++) {
                int[] held = (int[]) targets[i].mutable;
                if (held == null || held[0] != r || held[1] != i) {
                    throw new IllegalStateException(
                            "lost the old->young edge at round " + r + " target " + i);
                }
                acc += held[0] + held[1];
            }
        }

        // Keep the tree live to the end, and prove it is intact.
        if (sum(retained) != checksum) {
            throw new IllegalStateException("the retained tree changed under the collector");
        }
        System.out.println("retained=" + checksum + " churn=" + acc
                + " rounds=" + rounds + " writes=" + writes);
        System.out.println("PASS OldGenSparseCardProbe");
    }
}
