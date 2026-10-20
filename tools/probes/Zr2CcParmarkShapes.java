// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * zr z2/cc (2026-10-12): the parallel stop-the-world mark against two live-set SHAPES of the same
 * size, so an A/B separates "the engine does not scale" from "this graph never shares".
 *
 * <ul>
 *   <li>{@code narrow}: ONE complete binary tree of {@code 2^depth - 1} nodes (the
 *       {@code Zr1MkParmarkCost} shape). With claim-on-push a depth-first walk holds about one
 *       stack entry per level, so before the idle spill ({@code CRATONVM_ZGC_MARK_IDLE_SPILL}) the
 *       worker that took the root marked the whole tree alone.
 *   <li>{@code wide}: {@code 2^(depth-10)} trees of {@code 2^10 - 1} nodes each, all reachable
 *       from one {@code Object[]} -- the array's chunked scan seeds every worker at once, so the
 *       fixed valves share it with or without the idle spill (the control).
 * </ul>
 *
 * <p>The live set is built once; then {@code rounds} {@code System.gc()} calls (each a full
 * stop-the-world mark of the whole live set) bracket a little garbage. Read {@code drive_us=} /
 * {@code idle_spills=} off {@code [GC] zgc-mark-parallel:} and {@code mark_us=} off
 * {@code [GC] zgc-pause:} ({@code --verbose:gc}); CPU from {@code /usr/bin/time}.
 *
 * <p>Prints {@code ZR2CC-SHAPES shape=<s> nodes=<n> checksum=<c> ok} (HotSpot prints the same).
 */
public class Zr2CcParmarkShapes {
    static final class Node {
        Node left, right;
        final int depth;

        Node(int depth) {
            this.depth = depth;
        }
    }

    static Node build(int d) {
        Node n = new Node(d);
        if (d > 1) {
            n.left = build(d - 1);
            n.right = build(d - 1);
        }
        return n;
    }

    static long sum(Node root) {
        long c = 0;
        java.util.ArrayDeque<Node> st = new java.util.ArrayDeque<>();
        st.push(root);
        while (!st.isEmpty()) {
            Node x = st.pop();
            c += x.depth;
            if (x.left != null) st.push(x.left);
            if (x.right != null) st.push(x.right);
        }
        return c;
    }

    static long count(Node root) {
        long c = 0;
        java.util.ArrayDeque<Node> st = new java.util.ArrayDeque<>();
        st.push(root);
        while (!st.isEmpty()) {
            Node x = st.pop();
            c++;
            if (x.left != null) st.push(x.left);
            if (x.right != null) st.push(x.right);
        }
        return c;
    }

    public static void main(String[] args) {
        String shape = args.length > 0 ? args[0] : "narrow";
        int depth = args.length > 1 ? Integer.parseInt(args[1]) : 21;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 20;
        List<Node> roots = new ArrayList<>();
        Object[] holder;
        if (shape.equals("wide")) {
            int sub = Math.min(10, depth);
            int n = 1 << (depth - sub);
            holder = new Object[n];
            for (int i = 0; i < n; i++) {
                Node t = build(sub);
                holder[i] = t;
                roots.add(t);
            }
        } else {
            Node t = build(depth);
            holder = new Object[] {t};
            roots.add(t);
        }
        Object[] junk = new Object[1024];
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 1024; i++) {
                junk[i] = new int[16];
            }
            System.gc();
        }
        long nodes = 0, checksum = 0;
        for (Node t : roots) {
            nodes += count(t);
            checksum += sum(t);
        }
        long want = shape.equals("wide")
                ? ((1L << Math.min(10, depth)) - 1) * (1L << (depth - Math.min(10, depth)))
                : (1L << depth) - 1;
        System.out.println("ZR2CC-SHAPES shape=" + shape + " nodes=" + nodes + " checksum=" + checksum
                + (nodes == want && holder.length > 0 ? " ok" : " BAD"));
    }
}
