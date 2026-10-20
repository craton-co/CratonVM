// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w5/pinned5 (2026-09-24): a JIT-WARM, multi-threaded young churn for
 * the pinned in-place young copy ({@code CRATONVM_GEN_PINNED_YOUNG_COPY}).
 *
 * <p>Every worker spends its life in one hot method that keeps an array and
 * a young list live in locals ACROSS allocations inside its loops — the
 * array base/element addresses and the list head are exactly the
 * unrewritable conservative words (interior and derived included) that make
 * the conservative-JIT-root term ({@code nonmoving-unrewritable-conservative-jit-roots})
 * divert every JIT-warm young cycle. Each call also replaces one slot of a
 * per-thread RING of 4096 entries, so a medium-lived population survives many
 * young collections: it is copied (or pinned, or promoted) cycle after cycle,
 * and the final verification pass re-derives every byte and every list value
 * of every ring entry from its seed. A lost, stale or half-copied survivor is
 * a {@code FAIL}, not a different checksum.
 *
 * <p>Per-thread results are independent and summed, so the checksum does not
 * depend on scheduling or on the collector.
 *
 * <p>Expected output (HotSpot 25, {@code -XX:+UseSerialGC -Xmx256m}):
 * <pre>
 *   PASS pinned threads=4 calls=600000 bad=0 checksum=-6144781693192940271
 *   (args 1 600000)  PASS pinned threads=1 calls=600000 bad=0 checksum=5088764178355511906
 *   (args 4 100000)  PASS pinned threads=4 calls=100000 bad=0 checksum=8172764217083875233
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR4W5PinnedYoungCopyProbe
 *   CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W5PinnedYoungCopyProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W5PinnedYoungCopyProbe
 *   CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_DBG_GC_STRESS=250000 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W5PinnedYoungCopyProbe 4 100000
 * </pre>
 * What to read with the flag: {@code [GC] decision histogram:} must show a
 * {@code moving-pinned-pages} row above zero, and {@code [GC] young_pinned_copy:}
 * its {@code pycopy_cycles} equal to that row. Without the flag the same run
 * is the control: {@code moving-pinned-pages} absent, every JIT-warm cycle
 * {@code nonmoving-unrewritable-conservative-jit-roots} as in wave 4. Optional
 * args: {@code threads calls}. Exit status 1 on FAIL.
 */
public final class GenR4W5PinnedYoungCopyProbe {
    static final int RING = 4096;

    static final class Node {
        final int v;
        final Node next;
        final byte[] payload;

        Node(int v, Node next, byte[] payload) {
            this.v = v;
            this.next = next;
            this.payload = payload;
        }
    }

    static int len(int seed) {
        return 64 + (seed & 63);
    }

    static byte at(int seed, int k) {
        return (byte) (seed * 31 + k);
    }

    /**
     * The hot method. {@code buf} (and an element address into it) and
     * {@code head} are live across every allocation in the first loop, and the
     * {@code char[]}/{@code byte[]} string loops run between them.
     */
    static long work(int seed, Node[] ring, int slot) {
        final byte[] buf = new byte[len(seed)];
        Node head = null;
        long acc = 0;
        for (int k = 0; k < buf.length; k++) {
            buf[k] = at(seed, k);
            if ((k & 7) == 0) {
                head = new Node(k, head, null);
            }
            acc = acc * 131 + buf[k];
        }
        final StringBuilder sb = new StringBuilder(40);
        int x = seed;
        for (int k = 0; k < 32; k++) {
            x = x * 1103515245 + 12345;
            sb.append((char) ('a' + ((x >>> 16) & 15)));
        }
        final String s = sb.toString();
        final byte[] bytes = s.getBytes(java.nio.charset.StandardCharsets.ISO_8859_1);
        for (byte b : bytes) {
            acc = acc * 131 + b;
        }
        // Medium-lived: survives until the ring comes round again.
        ring[slot] = new Node(seed, head, buf);
        for (Node c = head; c != null; c = c.next) {
            acc += c.v;
        }
        return acc;
    }

    /** Re-derive one ring entry from its seed; -1 when anything is off. */
    static long verify(Node n) {
        if (n == null) {
            return 0;
        }
        final int seed = n.v;
        final byte[] p = n.payload;
        if (p == null || p.length != len(seed)) {
            return -1;
        }
        long acc = seed;
        for (int k = 0; k < p.length; k++) {
            if (p[k] != at(seed, k)) {
                return -1;
            }
            acc = acc * 131 + p[k];
        }
        int expect = ((p.length - 1) / 8) * 8;
        for (Node c = n.next; c != null; c = c.next) {
            if (c.v != expect || c.payload != null) {
                return -1;
            }
            acc += c.v;
            expect -= 8;
        }
        if (expect != -8) {
            return -1;
        }
        return acc;
    }

    public static void main(String[] args) throws InterruptedException {
        final int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        final int calls = args.length > 1 ? Integer.parseInt(args[1]) : 600_000;
        final long[] results = new long[threads];
        final long[] bad = new long[threads];
        final Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                final Node[] ring = new Node[RING];
                long acc = 0;
                for (int i = 0; i < calls; i++) {
                    acc = acc * 7 + work(id * 1_000_003 + i, ring, i & (RING - 1));
                }
                long v = 0;
                long b = 0;
                for (Node n : ring) {
                    final long r = verify(n);
                    if (r == -1) {
                        b++;
                    } else {
                        v = v * 31 + r;
                    }
                }
                results[id] = acc ^ v;
                bad[id] = b;
            }, "pinned-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long checksum = 0;
        long badTotal = 0;
        for (int t = 0; t < threads; t++) {
            checksum += results[t];
            badTotal += bad[t];
        }
        final String line = "pinned threads=" + threads + " calls=" + calls + " bad=" + badTotal
                + " checksum=" + checksum;
        if (badTotal != 0) {
            System.out.println("FAIL " + line);
            System.exit(1);
        }
        System.out.println("PASS " + line);
    }
}
