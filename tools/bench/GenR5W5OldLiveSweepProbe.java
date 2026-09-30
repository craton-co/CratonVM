// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r5w5/old9 (2026-09-27): a stop-the-world major on a LARGE, SPARSE old
 * generation — the case the O(live) collection
 * ({@code CRATONVM_GC_OLD_LIVE_SWEEP}) exists for
 * ({@code docs/internal/gc/gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep-DONE-20260928.md}).
 *
 * <p>Builds {@code n} small nodes (default 2,000,000), tenures them with two
 * {@code System.gc()}, then drops all but one in {@code keepEvery} (default 20)
 * and times the {@code System.gc()} that reclaims the rest — a major whose
 * dead set is 95 % of the old generation. The walked collection decodes every
 * dead header three times (walk, closure, free loop); the O(live) one decodes
 * none. Then it checks every survivor, allocates a second generation of nodes
 * into the freed space, checks both, and times one more (steady-state) major.
 *
 * <p>stdout is deterministic and identical on HotSpot and CratonVM:
 * <pre>
 *   built nodes=2000000
 *   survivors ok count=100000 checksum=3099969700000
 *   reuse ok count=1000000
 *   PASS
 * </pre>
 * (for the defaults; the checksum is {@code sum(i * 31 + 7)} over the kept
 * indices). Timings go to stderr only, one line each:
 * {@code [probe] sparse-major ms=...} and {@code [probe] steady-major ms=...}.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx512m -cp tools/bench GenR5W5OldLiveSweepProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR5W5OldLiveSweepProbe
 *   CRATONVM_GC_OLD_LIVE_SWEEP=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR5W5OldLiveSweepProbe
 * </pre>
 * Compare the {@code sparse-major} lines of the last two (interleave the runs,
 * take medians). {@code RUST_LOG=cratonvm::gc=debug} shows one
 * {@code old-gen live sweep (O(live))} line per major the flag answered, and
 * {@code CRATONVM_DBG_OLD_LIVE_SWEEP_VERIFY=1} a
 * {@code [old-live-sweep] verify ... mismatches=0} line per major.
 */
public final class GenR5W5OldLiveSweepProbe {
    static final class Node {
        long v;
        Node next;

        Node(long v) {
            this.v = v;
        }
    }

    static final int CHUNK = 4096;

    /** Nodes in chunks of {@code CHUNK}, so no single array is humongous. */
    static Node[][] build(int n, long salt) {
        Node[][] chunks = new Node[(n + CHUNK - 1) / CHUNK][];
        for (int c = 0; c < chunks.length; c++) {
            int len = Math.min(CHUNK, n - c * CHUNK);
            Node[] chunk = new Node[len];
            for (int j = 0; j < len; j++) {
                long i = (long) c * CHUNK + j;
                chunk[j] = new Node(i * 31 + 7 + salt);
                if (j > 0) {
                    chunk[j - 1].next = chunk[j];
                }
            }
            chunks[c] = chunk;
        }
        return chunks;
    }

    static long timedGc() {
        long t0 = System.nanoTime();
        System.gc();
        return (System.nanoTime() - t0) / 1_000_000L;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2_000_000;
        int keepEvery = args.length > 1 ? Integer.parseInt(args[1]) : 20;
        Node[][] chunks = build(n, 0);
        System.gc();
        System.gc();
        System.out.println("built nodes=" + n);

        // Drop all but one in `keepEvery`: unlink the kept nodes from their
        // neighbours so the dropped ones are unreachable.
        for (Node[] chunk : chunks) {
            for (int j = 0; j < chunk.length; j++) {
                if (chunk[j] == null) {
                    continue;
                }
                chunk[j].next = null;
            }
        }
        long kept = 0;
        long checksum = 0;
        for (int c = 0; c < chunks.length; c++) {
            Node[] chunk = chunks[c];
            for (int j = 0; j < chunk.length; j++) {
                long i = (long) c * CHUNK + j;
                if (i % keepEvery != 0) {
                    chunk[j] = null;
                } else {
                    kept++;
                    checksum += i * 31 + 7;
                }
            }
        }
        System.err.println("[probe] sparse-major ms=" + timedGc());

        boolean ok = true;
        long seen = 0;
        long sum = 0;
        for (int c = 0; c < chunks.length; c++) {
            Node[] chunk = chunks[c];
            for (int j = 0; j < chunk.length; j++) {
                long i = (long) c * CHUNK + j;
                Node x = chunk[j];
                if (i % keepEvery == 0) {
                    if (x == null || x.v != i * 31 + 7 || x.next != null) {
                        ok = false;
                    } else {
                        seen++;
                        sum += x.v;
                    }
                } else if (x != null) {
                    ok = false;
                }
            }
        }
        ok &= seen == kept && sum == checksum;
        System.out.println(
                (ok ? "survivors ok" : "survivors FAILED") + " count=" + seen + " checksum=" + sum);

        // Reuse: a second generation of nodes lands in the freed space.
        int m = n / 2;
        Node[][] second = build(m, 1);
        System.gc();
        boolean reuse = true;
        long reuseSeen = 0;
        for (int c = 0; c < second.length; c++) {
            Node[] chunk = second[c];
            for (int j = 0; j < chunk.length; j++) {
                long i = (long) c * CHUNK + j;
                Node x = chunk[j];
                if (x == null || x.v != i * 31 + 8 || (j + 1 < chunk.length && x.next != chunk[j + 1])) {
                    reuse = false;
                } else {
                    reuseSeen++;
                }
            }
        }
        // The first generation's survivors are still intact beside it.
        for (int c = 0; c < chunks.length && reuse; c++) {
            Node[] chunk = chunks[c];
            for (int j = 0; j < chunk.length; j++) {
                long i = (long) c * CHUNK + j;
                if (i % keepEvery == 0 && (chunk[j] == null || chunk[j].v != i * 31 + 7)) {
                    reuse = false;
                }
            }
        }
        System.err.println("[probe] steady-major ms=" + timedGc());
        System.out.println((reuse ? "reuse ok" : "reuse FAILED") + " count=" + reuseSeen);
        boolean pass = ok && reuse;
        System.out.println(pass ? "PASS" : "FAIL");
        if (!pass) {
            System.exit(1);
        }
    }
}
