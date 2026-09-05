// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Builds a large, mostly-immutable OLD generation, then churns YOUNG garbage
 * against it without ever storing into the tenured set.
 *
 * This is the shape a generational collector is supposed to be good at and
 * the one that exposed `gc-genpause`: the old generation is large, it holds
 * (almost) no old-to-young edges, and a minor collection therefore has almost
 * nothing to do beyond copying the young survivors. Any phase whose cost
 * tracks OLD-generation size, or ALLOCATED young bytes rather than live ones,
 * shows up here as pure waste.
 *
 * Usage: OldGenRsetProbe [retainedDepth] [rounds] [churnDepth]
 *   retainedDepth  binary tree depth held for the whole run (default 18)
 *   rounds         churn iterations (default 40)
 *   churnDepth     depth of each discarded tree (default 12)
 *
 * The two checksums are printed so an A/B can be shown to have run the same
 * program. They are pure functions of the arguments, so any arm that prints
 * different numbers did different work.
 *
 * Suggested arms (see gen-gc-minor-pause-20260902.md):
 *   CRATONVM_DBG=gc-stats,gcpause ... -XX:+UseGenerationalGC -Xmx1g \
 *       -c . OldGenRsetProbe 19 700 16
 */
public final class OldGenRsetProbe {
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

    static long sum(Node n) {
        return n == null ? 0 : n.v + sum(n.a) + sum(n.b);
    }

    public static void main(String[] args) {
        int depth = args.length > 0 ? Integer.parseInt(args[0]) : 18;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int churnDepth = args.length > 2 ? Integer.parseInt(args[2]) : 12;

        // 1. Retained tree: tenures into old gen and stays there.
        Node retained = build(depth, 1);
        long checksum = sum(retained);

        // 2. Young churn: short-lived garbage, no stores into the old tree, so
        //    the card table stays clean and every old-to-young scan is waste.
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            Node garbage = build(churnDepth, r);
            acc += sum(garbage);
        }
        // Keep `retained` reachable to the very end.
        if (checksum == Long.MIN_VALUE) {
            System.out.println(retained);
        }
        System.out.println("retained=" + checksum + " churn=" + acc);
    }
}
