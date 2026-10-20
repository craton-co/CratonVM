// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w2/youngpolicy (2026-09-23): a workload built out of {@code System.gc()}
 * calls, for the A/B of {@code CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG}.
 *
 * <p>Every {@code System.gc()} on the Generational backend sweeps young IN
 * PLACE by default ({@code nonmoving-explicit-full-gc}), so the from-space
 * never compacts and fragments monotonically. With the flag set the same call
 * takes the moving (Cheney) cycle when nothing else diverts it. This probe keeps
 * a medium live set (a linked list of small nodes, re-linked every round so
 * survivors interleave with garbage) and churns short-lived arrays between
 * explicit collections. It is self-checking: the printed checksum is the same
 * on every JVM and every configuration, and a wrong one means a collection
 * lost or corrupted a live node.
 *
 * <pre>
 *   java -cp tools/bench GenR4w2SystemGcLoopProbe 200
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4w2SystemGcLoopProbe 200
 *   CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1 cratonvm -XX:+UseGenerationalGC -Xmx256m \
 *       -cp tools/bench GenR4w2SystemGcLoopProbe 200
 * </pre>
 * Read {@code [GC] decision histogram:} (with {@code CRATONVM_DBG=gc-stats}) for
 * engagement: {@code nonmoving-explicit-full-gc} must drop to ~0 under the flag.
 */
public final class GenR4w2SystemGcLoopProbe {
    static final class Node {
        final int value;
        Node next;

        Node(int value, Node next) {
            this.value = value;
            this.next = next;
        }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        final int live = 20_000;
        Node head = null;
        for (int i = 0; i < live; i++) {
            head = new Node(i, head);
        }
        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            // Garbage interleaved with a fresh copy of part of the live set.
            Node rebuilt = null;
            int k = 0;
            for (Node n = head; n != null; n = n.next) {
                sink = new byte[64 + (k & 127)];
                rebuilt = new Node(n.value, rebuilt);
                if ((k & 3) != 0) {
                    sink = new long[8];
                }
                k++;
            }
            // Restore original order so the checksum is round-independent.
            Node reversed = null;
            for (Node n = rebuilt; n != null; n = n.next) {
                reversed = new Node(n.value, reversed);
            }
            head = reversed;
            System.gc();
        }
        long ms = (System.nanoTime() - t0) / 1_000_000;
        long sum = 0;
        int count = 0;
        for (Node n = head; n != null; n = n.next) {
            sum += n.value;
            count++;
        }
        // 0 + 1 + ... + (live - 1)
        long expected = (long) live * (live - 1) / 2;
        System.out.println("nodes=" + count + " checksum=" + sum
                + (sum == expected && count == live ? " ok" : " FAIL expected=" + expected));
        System.err.println("elapsed_ms=" + ms + " rounds=" + rounds);
    }
}
