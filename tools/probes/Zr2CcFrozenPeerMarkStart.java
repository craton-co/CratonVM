// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicBoolean;

/**
 * zr z2/cc (2026-10-12): the positive control the take-over cover's rows never had.
 *
 * <p>{@code Zr1BrSatbTakeover} with {@code CRATONVM_ZGC_CONC_START=20} measured 399 mark-start
 * pauses and ZERO {@code [zgc-satb-window] mark start refused} lines: every compiled worker
 * reached its back-edge poll inside the take-over's first-pass grace
 * ({@code CRATONVM_XT_FIRST_PASS_GRACE_US}, default 200 us), so no peer was ever frozen and the
 * cover never acted. This probe keeps {@code WORKERS} threads inside one long call-free,
 * allocation-free compiled store loop (reference-array and reference-field stores over live
 * values of old objects) and is meant to run with the grace OFF
 * ({@code CRATONVM_XT_FIRST_PASS_GRACE_US=0}), so the take-over's round-0 pass suspends whatever
 * peer is in compiled code at that instant: frozen peers at mark start, on demand.
 *
 * <p>The main thread allocates 64 KiB garbage for {@code seconds} so the collector opens
 * concurrent cycles on its own. At the end every ring slot is checked; a node freed while
 * reachable reads back wrong. Prints {@code ZR2CC-FROZEN ok} (HotSpot prints the same).
 *
 * <pre>
 * Rows (orchestrator, Linux host; jdk-only default unless noted):
 *   cover:  CRATONVM_ZGC_CONC_START=20 CRATONVM_XT_FIRST_PASS_GRACE_US=0 CRATONVM_DBG_ZGC_SATB_WINDOW=1
 *           cratonvm -Xmx256m --verbose:gc Zr2CcFrozenPeerMarkStart 8
 *           pass: "ZR2CC-FROZEN ok" and >= 1 "[zgc-satb-window] mark start refused" line
 *   kill:   cover + CRATONVM_ZGC_SATB_TAKEOVER_COVER=0: "ok", no refusal line, mark=concurrent cycles
 *   grace:  cover without CRATONVM_XT_FIRST_PASS_GRACE_US=0: the z1 shape (refusals expected ~0)
 *   compat: cover with --compatible
 *   HotSpot: java -XX:+UseZGC -Xmx256m Zr2CcFrozenPeerMarkStart 8
 * </pre>
 */
public class Zr2CcFrozenPeerMarkStart {
    static final int RING = 1 << 14;
    static final int WORKERS = 3;

    static final class Node {
        Node link;
        final long sum;
        final long[] payload;

        Node(long seed) {
            this.sum = seed * 31 + 7;
            this.payload = new long[] {seed, sum};
        }

        boolean ok() {
            return payload.length == 2 && payload[1] == sum && payload[0] * 31 + 7 == sum;
        }
    }

    static final Node[] ring = new Node[RING];
    static final Node[][] slots = new Node[WORKERS][RING];
    static final AtomicBoolean stop = new AtomicBoolean();

    /** Call-free and allocation-free: the whole body is loads and reference stores. */
    static long storeLoop(Node[] mine, int start, int rounds) {
        long acc = 0;
        int mask = RING - 1;
        for (int r = 0; r < rounds; r++) {
            int i = (start + r) & mask;
            int j = (start + r * 7 + 1) & mask;
            Node a = ring[i];
            Node b = ring[j];
            // Reference-array store over a live value.
            mine[i] = b;
            // Reference-field store over a live value of an old object.
            a.link = b.link != null ? b.link : b;
            acc += a.sum;
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        for (int i = 0; i < RING; i++) {
            ring[i] = new Node(i);
        }
        for (int i = 0; i < RING; i++) {
            ring[i].link = ring[(i + 1) & (RING - 1)];
        }
        Thread[] ts = new Thread[WORKERS];
        long[] sink = new long[WORKERS];
        for (int w = 0; w < WORKERS; w++) {
            final int id = w;
            ts[w] = new Thread(() -> {
                int start = id * 977;
                while (!stop.get()) {
                    sink[id] += storeLoop(slots[id], start, 5_000_000);
                    start += 13;
                }
            });
            ts[w].setDaemon(true);
            ts[w].start();
        }
        long deadline = System.nanoTime() + seconds * 1_000_000_000L;
        long garbage = 0;
        Object[] keep = new Object[64];
        int k = 0;
        while (System.nanoTime() < deadline) {
            byte[] b = new byte[64 * 1024];
            keep[k++ & 63] = b;
            garbage += b.length;
        }
        stop.set(true);
        for (Thread t : ts) {
            t.join();
        }
        for (int i = 0; i < RING; i++) {
            Node n = ring[i];
            if (!n.ok() || (n.link != null && !n.link.ok())) {
                System.out.println("ZR2CC-FROZEN BAD ring " + i);
                return;
            }
            for (int w = 0; w < WORKERS; w++) {
                Node s = slots[w][i];
                if (s != null && !s.ok()) {
                    System.out.println("ZR2CC-FROZEN BAD slot " + w + " " + i);
                    return;
                }
            }
        }
        System.out.println("ZR2CC-FROZEN " + (garbage > 0 ? "ok" : "BAD no garbage"));
    }
}
