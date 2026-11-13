// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Fixed-work twin of {@code R23W6KeepLong}: runs exactly {@code rounds} rounds
 * of 64 chains of {@code Node} + {@code byte[224]} (2_400_000 nodes, about
 * 634 MB live) under {@code -Xmx1g}, so a loaded host compares CPU and wall
 * per round instead of rounds per wall second. Usage:
 * {@code java -XX:+UseG1GC -Xmx1g G1epKeepRounds [rounds] [nodesPerRound]}.
 */
public class G1epKeepRounds {
    static final class Node {
        final Node next;
        final byte[] payload;
        final int tag;

        Node(Node next, int size, int tag) {
            this.next = next;
            this.payload = new byte[size];
            this.payload[0] = (byte) tag;
            this.payload[size - 1] = (byte) (tag >>> 8);
            this.tag = tag;
        }
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        int perRound = args.length > 1 ? Integer.parseInt(args[1]) : 2_400_000;
        long t0 = System.nanoTime();
        boolean ok = true;
        long best = Long.MAX_VALUE;
        for (int r = 0; r < rounds; r++) {
            long r0 = System.nanoTime();
            Node[] ring = new Node[64];
            for (int i = 0; i < perRound; i++) {
                ring[i & 63] = new Node(ring[i & 63], 224, i);
            }
            for (int s = 0; s < 64; s++) {
                int expect = ((perRound - 1 - s) / 64) * 64 + s;
                for (Node n = ring[s]; n != null; n = n.next) {
                    if (n.tag != expect || n.payload[0] != (byte) expect
                            || n.payload[223] != (byte) (expect >>> 8)) {
                        ok = false;
                    }
                    expect -= 64;
                }
                if (expect != s - 64) {
                    ok = false;
                }
            }
            best = Math.min(best, System.nanoTime() - r0);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        System.out.println("rounds=" + rounds + " ms=" + ms + " ms_per_round=" + (ms / Math.max(1, rounds))
                + " best_round_ms=" + best / 1_000_000);
        System.out.println("fact chains_ok=" + ok);
    }
}
