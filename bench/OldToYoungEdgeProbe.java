// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The companion to {@code OldGenRsetProbe}: a workload that produces a large
 * number of REAL old-to-young references, and then proves every one of them
 * survived.
 *
 * {@code OldGenRsetProbe} deliberately has none, which makes it the right
 * probe for pricing the old-to-young scans and the wrong one for trusting the
 * card table. This is the other half. It tenures an array of nodes, then
 * repeatedly overwrites their reference fields with freshly allocated young
 * objects — every one of those stores is an old-to-young edge the write
 * barrier must record, and every minor collection in between must find it or
 * reclaim a reachable object.
 *
 * The verification is the point: each payload carries a value derived from
 * where it was stored, and the final pass recomputes it. A dropped card shows
 * up as a mismatch or a NullPointerException, not as a silent slowdown.
 *
 * Usage: OldToYoungEdgeProbe [nodes] [rounds] [churnDepth]
 *   nodes       tenured nodes holding young referents (default 20000)
 *   rounds      overwrite passes over the whole array (default 60)
 *   churnDepth  depth of the garbage tree built between passes (default 14)
 *
 * Run it with the remembered-set verifier armed; `edges` must be non-zero, or
 * the run proves nothing:
 *
 *   CRATONVM_GC_VERIFY_RSET=1 cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx512m -c bench OldToYoungEdgeProbe
 *
 * See performance/gen-gc-minor-pause-20260902.md.
 */
public final class OldToYoungEdgeProbe {
    static final class Node {
        Object payload;
        int id;
        Node(int id) { this.id = id; }
    }

    static final class Payload {
        final long stamp;
        Payload(long stamp) { this.stamp = stamp; }
    }

    static final class Tree {
        Tree a, b;
        int v;
        Tree(int v) { this.v = v; }
    }

    static Tree build(int depth, int v) {
        Tree t = new Tree(v);
        if (depth > 0) {
            t.a = build(depth - 1, v * 2);
            t.b = build(depth - 1, v * 2 + 1);
        }
        return t;
    }

    static long sum(Tree t) {
        return t == null ? 0 : t.v + sum(t.a) + sum(t.b);
    }

    static long stampFor(int node, int round) {
        return ((long) node << 20) ^ (round * 2654435761L);
    }

    public static void main(String[] args) {
        int nodes = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        int churnDepth = args.length > 2 ? Integer.parseInt(args[2]) : 14;

        // 1. The tenured side. Allocated once and held for the whole run, so a
        //    few minor collections promote it to old gen.
        Node[] held = new Node[nodes];
        for (int i = 0; i < nodes; i++) {
            held[i] = new Node(i);
        }

        // 2. Churn until the array and its nodes have certainly tenured. Each
        //    tree is garbage immediately, so this is pure minor-GC pressure.
        long warm = 0;
        for (int r = 0; r < 4; r++) {
            warm += sum(build(churnDepth, r));
        }

        // 3. The edges. Each store puts a BRAND NEW young object into a field
        //    of a tenured node: exactly the old-to-young reference a card
        //    table exists to remember. Garbage between passes forces minor
        //    collections while those references are the only thing keeping the
        //    payloads alive.
        int lastRound = -1;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < nodes; i++) {
                held[i].payload = new Payload(stampFor(i, r));
            }
            lastRound = r;
            warm += sum(build(churnDepth, r));
        }

        // 4. Verify. Every payload must still be the one the last pass stored.
        //    A card the collector failed to remember reclaims the payload while
        //    its tenured referrer still points at it, which surfaces here as a
        //    null, a wrong stamp, or a wild dereference.
        int checked = 0;
        for (int i = 0; i < nodes; i++) {
            Object p = held[i].payload;
            if (!(p instanceof Payload)) {
                throw new IllegalStateException(
                    "node " + i + " lost its payload: " + p
                        + " (an old-to-young edge was not remembered)");
            }
            long want = stampFor(i, lastRound);
            long got = ((Payload) p).stamp;
            if (got != want) {
                throw new IllegalStateException(
                    "node " + i + " stamp " + got + " != " + want
                        + " (a reclaimed payload was replaced by another object)");
            }
            checked++;
        }
        System.out.println("edges_verified=" + checked + " rounds=" + rounds
            + " warm=" + warm);
    }
}
