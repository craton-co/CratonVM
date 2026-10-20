// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicBoolean;

/**
 * zr z1/br: witness for
 * docs/internal/fixed-bugs/zgc-zr1br-takeover-straddles-the-satb-gate-FIXED-20261017.md.
 *
 * <p>Worker threads run a hot, call-free compiled loop that overwrites reference fields and
 * reference-array elements of a long-lived ring (old objects whose previous values are dropped)
 * and allocates fresh nodes into it. The loop has no call in its body, so a mark-start pause has
 * to take the workers over in compiled code. The main thread allocates garbage until the heap
 * crosses the concurrent-start threshold several times.
 *
 * <p>At the end every ring slot is walked and checked: each node must still carry the checksum it
 * was built with. A node freed while reachable reads back as a wrong checksum, a wrong class, or a
 * crash. Prints {@code OK checked=true garbageMB=true} on success (HotSpot: always; the line is
 * deterministic so the two can be diffed).
 *
 * <pre>
 * Rows (orchestrator, Linux host; see the page for the pass criteria):
 *   cover:   CRATONVM_ZGC_CONC_START=20 CRATONVM_DBG_ZGC_SATB_WINDOW=1 cratonvm -Xmx256m Zr1BrSatbTakeover
 *            expect OK and >= 1 "[zgc-satb-window] mark start refused" line (positive control)
 *   kill:    the same + CRATONVM_ZGC_SATB_TAKEOVER_COVER=0   expect OK and no refusal line
 *   quiet:   cover row + CRATONVM_XT_JIT_ROOT_SCAN=0          expect no refusal line
 *   compat:  cover row with --compatible                      expect as cover
 *   HotSpot: java -XX:+UseZGC -Xmx256m Zr1BrSatbTakeover       expect OK
 * </pre>
 */
public class Zr1BrSatbTakeover {
    static final int RING = 4096;
    static final int WORKERS = 3;

    static final class Node {
        Node next;
        Object payload;
        final long sum;

        Node(long seed) {
            this.sum = seed * 31 + 7;
            this.payload = new long[] {seed, sum};
        }

        boolean ok() {
            Object p = payload;
            if (!(p instanceof long[])) {
                return false;
            }
            long[] a = (long[]) p;
            return a.length == 2 && a[1] == sum && a[0] * 31 + 7 == sum;
        }
    }

    static final Node[] ring = new Node[RING];
    static final Node[][] slots = new Node[WORKERS][RING];
    static final AtomicBoolean stop = new AtomicBoolean();

    /** The hot body: no calls besides the allocation, so it compiles into one loop. */
    static long churn(Node[] mine, long seed, int rounds) {
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            int i = (int) ((seed + r) & (RING - 1));
            Node fresh = new Node(seed + r);
            Node old = mine[i];
            // Reference-array store over a live value.
            mine[i] = fresh;
            // Reference-field store over a live value of an old object.
            Node holder = ring[i];
            holder.next = old != null ? old : fresh;
            acc += fresh.sum;
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < RING; i++) {
            ring[i] = new Node(-i - 1);
        }
        Thread[] ts = new Thread[WORKERS];
        long[] sink = new long[WORKERS];
        for (int w = 0; w < WORKERS; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                long seed = id * 1_000_003L;
                while (!stop.get()) {
                    sink[id] += churn(slots[id], seed, 200_000);
                    seed += 200_000;
                }
            });
            ts[w].setDaemon(true);
            ts[w].start();
        }
        long garbage = 0;
        long deadline = System.nanoTime() + 6_000_000_000L;
        while (System.nanoTime() < deadline) {
            byte[] b = new byte[64 * 1024];
            garbage += b.length;
        }
        stop.set(true);
        for (Thread t : ts) {
            t.join();
        }
        int checked = 0;
        for (int i = 0; i < RING; i++) {
            Node h = ring[i];
            if (!h.ok()) {
                System.out.println("BAD ring " + i);
                return;
            }
            checked++;
            if (h.next != null) {
                if (!h.next.ok()) {
                    System.out.println("BAD ring.next " + i);
                    return;
                }
                checked++;
            }
            for (int w = 0; w < WORKERS; w++) {
                Node n = slots[w][i];
                if (n != null) {
                    if (!n.ok()) {
                        System.out.println("BAD slot " + w + " " + i);
                        return;
                    }
                    checked++;
                }
            }
        }
        System.out.println("OK checked=" + (checked > 0) + " garbageMB=" + (garbage >> 20 > 0));
    }
}
