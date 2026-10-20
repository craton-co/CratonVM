// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * zr z4/rv4: a FAILED parallel stop-the-world mark must not free objects the
 * driver claimed but never scanned.
 *
 * <p>Keeps a long singly linked list and a binary tree live through a static
 * root, churns garbage so ZGC collects many times, and checks both structures
 * after every round. With {@code CRATONVM_DBG_ZGC_PARMARK_FAIL=1} every
 * parallel mark claims its roots and fails; the serial fallback then has to
 * trace below them ({@code CRATONVM_ZGC_PARMARK_FALLBACK_RESCAN}, default ON).
 *
 * <p>Usage: {@code Zr4Rv4ParmarkFallback [listLen] [treeDepth] [rounds]}.
 * Prints {@code result ok rounds=<n>} or {@code result BAD ...} and exits 1.
 */
public class Zr4Rv4ParmarkFallback {
    static final class Node {
        Node next;
        long value;
        Node(long v) { value = v; }
    }

    static final class Tree {
        Tree left, right;
        int depth;
        Tree(int d) {
            depth = d;
            if (d > 0) {
                left = new Tree(d - 1);
                right = new Tree(d - 1);
            }
        }
        long count() {
            long n = 1;
            if (left != null) n += left.count();
            if (right != null) n += right.count();
            return n;
        }
    }

    static Node head;
    static Tree tree;
    static Object sink;

    public static void main(String[] args) {
        int listLen = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int depth = args.length > 1 ? Integer.parseInt(args[1]) : 16;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 30;

        Node tail = null;
        for (int i = 0; i < listLen; i++) {
            Node n = new Node(i);
            if (head == null) head = n; else tail.next = n;
            tail = n;
        }
        tree = new Tree(depth);
        long expectedSum = (long) listLen * (listLen - 1) / 2;
        long expectedTree = (1L << (depth + 1)) - 1;

        for (int r = 0; r < rounds; r++) {
            // Garbage: enough to force several collections per round.
            for (int i = 0; i < 200_000; i++) {
                sink = new long[16];
            }
            long sum = 0;
            long len = 0;
            for (Node n = head; n != null; n = n.next) {
                sum += n.value;
                len++;
            }
            long tc = tree.count();
            if (len != listLen || sum != expectedSum || tc != expectedTree) {
                System.out.println("result BAD round=" + r + " len=" + len + "/" + listLen
                        + " sum=" + sum + "/" + expectedSum + " tree=" + tc + "/" + expectedTree);
                System.exit(1);
            }
        }
        System.out.println("result ok rounds=" + rounds);
    }
}
