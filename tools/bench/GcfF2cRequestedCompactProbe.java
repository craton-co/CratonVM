// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;

/**
 * gcf f2/c (2026-10-01): {@code System.gc()} on a FRAGMENTED old generation,
 * checked for integrity after every collection. The deciding probe of
 * {@code CRATONVM_GC_REQUESTED_MAJOR_COMPACTS} (a requested major compacts the
 * old generation, as HotSpot Serial's full collection always does):
 * {@code docs/internal/gc/gcf-f1c-stw-full-gc-never-compacts-old-gen-by-default-CLOSED-20261012.md}.
 *
 * <p>A table of {@code nodes} nodes, each with an {@code int[]} payload of a
 * size that varies with its id and a link to another node, is built and aged
 * into the old generation (churn between explicit collections). Each round
 * then replaces every third node (a different third each round) with a fresh
 * one, relinks the table, churns, and calls {@code System.gc()}: the replaced
 * nodes die interleaved with the survivors, so the old generation is left
 * full of holes, which only a compaction removes. After every collection the
 * probe checks, for every node: its payload contents, its link (reference
 * equality with the node the table says it names, and that node's id), that
 * a {@code WeakReference} to it still answers it, and that its identity hash
 * code is the one recorded when it was created (a compaction that moved it
 * must carry the hash along). Any mismatch is counted in {@code bad}.
 *
 * <p>Stdout is one deterministic line; HotSpot ({@code -XX:+UseSerialGC
 * -Xmx64m}) prints
 * <pre>
 *   requested-compact nodes=60000 rounds=12 bad=0 checksum=...
 * </pre>
 * (the checksum depends only on the arguments). Stderr: the committed heap
 * after the last collection ({@code Runtime.totalMemory()}), for the
 * footprint comparison; never diffed.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx64m -cp tools/bench GcfF2cRequestedCompactProbe
 *   CRATONVM_GC_REQUESTED_MAJOR_COMPACTS=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx64m --nojit -cp tools/bench GcfF2cRequestedCompactProbe
 * </pre>
 * Optional arguments: {@code nodes} (default 60000), {@code rounds} (default 12).
 */
public final class GcfF2cRequestedCompactProbe {
    static final class Node {
        final int id;
        final int[] payload;
        Node next;
        final int hash;

        Node(int id) {
            this.id = id;
            this.payload = new int[8 + id % 29];
            for (int j = 0; j < payload.length; j++) {
                payload[j] = id * 31 + j;
            }
            this.hash = System.identityHashCode(this);
        }

        boolean intact() {
            if (payload.length != 8 + id % 29) {
                return false;
            }
            for (int j = 0; j < payload.length; j++) {
                if (payload[j] != id * 31 + j) {
                    return false;
                }
            }
            return System.identityHashCode(this) == hash;
        }
    }

    static Node[] table;
    @SuppressWarnings("unchecked")
    static WeakReference<Node>[] weak;
    static long sink;

    static int linkOf(int i, int n) {
        return (int) ((i * 7L + 1) % n);
    }

    static void relink(int n) {
        for (int i = 0; i < n; i++) {
            table[i].next = table[linkOf(i, n)];
        }
    }

    /** Short-lived garbage, so young collections run and the table ages. */
    static void churn(int megabytes) {
        for (int k = 0; k < megabytes * 64; k++) {
            byte[] b = new byte[16 * 1024 - 16];
            b[k % b.length] = (byte) k;
            sink += b[(k * 7) % b.length];
        }
    }

    static int verify(int n) {
        int bad = 0;
        for (int i = 0; i < n; i++) {
            Node x = table[i];
            Node want = table[linkOf(i, n)];
            if (!x.intact() || x.next != want || x.next.id != want.id || weak[i].get() != x) {
                bad++;
            }
        }
        return bad;
    }

    @SuppressWarnings("unchecked")
    public static void main(String[] args) {
        final int n = args.length > 0 ? Integer.parseInt(args[0]) : 60_000;
        final int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 12;
        table = new Node[n];
        weak = (WeakReference<Node>[]) new WeakReference[n];
        for (int i = 0; i < n; i++) {
            table[i] = new Node(i);
            weak[i] = new WeakReference<>(table[i]);
        }
        relink(n);
        // Age the table into the old generation.
        for (int k = 0; k < 4; k++) {
            churn(16);
            System.gc();
        }
        int bad = verify(n);
        int nextId = n;
        for (int r = 0; r < rounds; r++) {
            for (int i = r % 3; i < n; i += 3) {
                table[i] = new Node(nextId++);
                weak[i] = new WeakReference<>(table[i]);
            }
            relink(n);
            churn(16);
            System.gc();
            bad += verify(n);
        }
        long checksum = 0;
        for (int i = 0; i < n; i++) {
            Node x = table[i];
            checksum = checksum * 1_000_003L + x.id * 17L + x.next.id + x.payload.length;
        }
        System.out.println(new StringBuilder("requested-compact nodes=").append(n)
                .append(" rounds=").append(rounds)
                .append(" bad=").append(bad)
                .append(" checksum=").append(checksum).toString());
        System.err.println("[probe] committed_after=" + Runtime.getRuntime().totalMemory()
                + " sink=" + sink);
    }
}
