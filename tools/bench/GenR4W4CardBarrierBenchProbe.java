// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Generational GC round 4, wave 4, lane "cards4": a BARRIER-HEAVY
 * microbenchmark for the inline generational post barrier
 * ({@code CRATONVM_JIT_INLINE_CARD_MARK=1}).
 *
 * A tenured graph — one wide {@code Object[]} spanning hundreds of cards and a
 * few thousand tenured {@code Node}s — is mutated with YOUNG references from
 * two tiny, hot, compiled store methods ({@link #put} is an {@code aastore},
 * {@link #link} a {@code putfield}), with garbage churn between rounds so
 * young collections run while the only path to every young {@code Leaf} is an
 * old-to-young edge. Every round is verified; a lost card shows up as a
 * mismatch (or a crash), and the last line is deterministic:
 *
 * <pre>
 *   slots=S holders=H rounds=R mismatches=0 checksum=C
 * </pre>
 *
 * HotSpot prints the same line, and it is the ONLY stdout line of a passing
 * run (gce e2/y: the timing moved to stderr, so a battery can compare stdout
 * with HotSpot byte for byte). The A/B number is one STDERR line,
 * {@code [store-timing] rounds=R store_ms=T median_round_us=M steady_median_round_us=S},
 * the time spent in the store loops (the sum, and the median of the per-round
 * times, over all rounds and over all but the first tenth); it is not
 * deterministic. Compare {@code steady_median_round_us} across interleaved runs.
 *
 * Usage: GenR4W4CardBarrierBenchProbe [slotsLog2] [holders] [rounds] [churnDepth]
 *   slotsLog2   log2 of the wide array's length (default 16 = 65536 slots)
 *   holders     tenured Node count (default 4096)
 *   rounds      store+verify rounds (default 200)
 *   churnDepth  garbage tree depth per round (default 10)
 *
 * Commands (see docs/internal/reviews/gengc-round4-w4-cards4-20260924.md):
 *   javac -d tools/bench tools/bench/GenR4W4CardBarrierBenchProbe.java
 *   java -cp tools/bench GenR4W4CardBarrierBenchProbe            # oracle
 *   CRATONVM_GC_VERIFY_RSET=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC \
 *       -Xmx512m -c tools/bench GenR4W4CardBarrierBenchProbe                         # A
 *   CRATONVM_JIT_INLINE_CARD_MARK=1 CRATONVM_GC_VERIFY_RSET=1 cratonvm ... (same)    # B
 */
public final class GenR4W4CardBarrierBenchProbe {
    static final class Leaf {
        final long v;
        Leaf(long v) { this.v = v; }
    }

    static final class Node {
        Object ref;
        final int id;
        Node(int id) { this.id = id; }
    }

    static final class Tree {
        Tree a, b;
        int v;
        Tree(int v) { this.v = v; }
    }

    /** The compiled {@code aastore} under test. Kept tiny so it is hot early. */
    static void put(Object[] a, int i, Object v) {
        a[i] = v;
    }

    /** The compiled reference {@code putfield} under test. */
    static void link(Node n, Object v) {
        n.ref = v;
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

    static long slotKey(int round, int slot) {
        return ((long) round << 32) ^ (slot * 0x9E3779B1L);
    }

    static long nodeKey(int round, int id) {
        return ~(((long) round << 24) ^ (id * 0x85EBCA6BL));
    }

    public static void main(String[] args) {
        int log2 = args.length > 0 ? Integer.parseInt(args[0]) : 16;
        int holders = args.length > 1 ? Integer.parseInt(args[1]) : 4096;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        int churnDepth = args.length > 3 ? Integer.parseInt(args[3]) : 10;
        int n = 1 << log2;
        int mask = n - 1;

        Object[] wide = new Object[n];
        Node[] nodes = new Node[holders];
        for (int j = 0; j < holders; j++) {
            nodes[j] = new Node(j);
        }
        // Tenure the graph: survive enough young collections to be promoted.
        long sink = 0;
        for (int w = 0; w < 8; w++) {
            sink += sum(build(churnDepth + 3, w));
        }
        System.gc();

        long storeNanos = 0;
        final long[] roundNanos = new long[rounds];
        long checksum = 0;
        long mismatches = 0;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            // A scattered walk (odd stride: a permutation of the slots), so the
            // stores touch every card of the wide array in a cache-unfriendly
            // order, as a real remembered set does.
            for (int i = 0; i < n; i++) {
                int slot = (i * 7919) & mask;
                put(wide, slot, new Leaf(slotKey(r, slot)));
            }
            for (int j = 0; j < holders; j++) {
                link(nodes[j], new Leaf(nodeKey(r, j)));
            }
            final long dt = System.nanoTime() - t0;
            roundNanos[r] = dt;
            storeNanos += dt;

            // Young collections happen here: the Leafs are reachable only
            // through old-to-young edges.
            sink += sum(build(churnDepth, r));

            for (int k = 0; k < n; k++) {
                Object o = wide[k];
                long want = slotKey(r, k);
                if (!(o instanceof Leaf) || ((Leaf) o).v != want) {
                    if (mismatches < 10) {
                        System.out.println("MISMATCH wide slot=" + k + " round=" + r);
                    }
                    mismatches++;
                } else {
                    checksum += want & 0xFFFF;
                }
            }
            for (int j = 0; j < holders; j++) {
                Object o = nodes[j].ref;
                long want = nodeKey(r, j);
                if (!(o instanceof Leaf) || ((Leaf) o).v != want) {
                    if (mismatches < 10) {
                        System.out.println("MISMATCH node=" + j + " round=" + r);
                    }
                    mismatches++;
                } else {
                    checksum += (want >>> 7) & 0xFFFF;
                }
            }
        }
        if (sink == 42) {
            System.out.println("unreachable");
        }
        System.out.println("slots=" + n + " holders=" + holders + " rounds=" + rounds
                + " mismatches=" + mismatches + " checksum=" + checksum);
        // gce e2/y: stderr, so the stdout verdict is deterministic.
        System.err.println("[store-timing] rounds=" + rounds + " store_ms=" + (storeNanos / 1_000_000)
                + " median_round_us=" + medianUs(roundNanos, 0)
                + " steady_median_round_us=" + medianUs(roundNanos, rounds / 10));
    }

    /** Median of {@code nanos[from..]} in microseconds; 0 for an empty range. */
    static long medianUs(long[] nanos, int from) {
        if (from >= nanos.length) {
            return 0;
        }
        final long[] c = java.util.Arrays.copyOfRange(nanos, from, nanos.length);
        java.util.Arrays.sort(c);
        final int m = c.length / 2;
        final long med = c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2;
        return med / 1_000;
    }
}
