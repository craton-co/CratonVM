// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zgc round zr, wave z3, lane cs (2026-10-12).
//
// Soundness and engagement probe for CRATONVM_ZGC_CONC_EVERY_TRIGGER (every
// non-urgent ZGC trigger opens a concurrent mark; the collection follows the
// mark's fixed point). Under the switch far more of a run's collections end a
// concurrent phase than under CRATONVM_ZGC_CONC_START=auto, so the SATB
// barrier, allocate-black and the mark-end replay see far more traffic; this
// probe makes every one of those paths carry live data:
//
// * each worker owns two Object[] tables and MOVES its nodes between them
//   (`b[j] = a[j]; a[j] = null`), alternating direction every round -- the
//   classic snapshot-at-the-beginning hazard (a reference leaves an unscanned
//   slot for a scanned one; only the pre-write barrier keeps it marked);
// * every node is re-linked (`next`) to another live node each round, so
//   field stores overwrite references as well as array stores;
// * fresh replacement nodes are allocated during the phase (allocate-black);
// * garbage is allocated in between (small arrays; with `large`, 512 KiB
//   arrays too, which reach the headroom clause on small heaps).
//
// A swept-and-reused node reads back wrong (its id is stored three ways).
// Iteration counts are fixed, so stdout is deterministic.
//
// Usage: Zr3CsEveryTrigger <threads> <rounds> <liveMiB> [large]
// Output (HotSpot prints the same):
//   ZR3CS-EVERY threads=<t> rounds=<r> live=<MiB> large=<bool> lost=0 corrupt=0 ok
public class Zr3CsEveryTrigger {
    static final class Node {
        final int id;
        Node next;
        final int[] payload;

        Node(int id) {
            this.id = id;
            this.payload = new int[] {id, ~id, id * 31};
        }

        boolean intact() {
            return payload.length == 3 && payload[0] == id && payload[1] == ~id && payload[2] == id * 31;
        }
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        int liveMiB = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        boolean large = args.length > 3 && args[3].equals("large");
        // ~96 bytes per node (object + payload array + table slot).
        int n = Math.max(1024, (int) ((long) liveMiB * 1024 * 1024 / 96 / threads));
        int[] lost = new int[threads];
        int[] corrupt = new int[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int me = t;
            ts[t] = new Thread(() -> {
                Object[] a = new Object[n];
                Object[] b = new Object[n];
                for (int i = 0; i < n; i++) a[i] = new Node(i);
                Object[] junk = new Object[256];
                int jk = 0;
                long seed = 0x9E3779B97F4A7C15L ^ me;
                for (int r = 0; r < rounds; r++) {
                    Object[] from = (r & 1) == 0 ? a : b;
                    Object[] to = (r & 1) == 0 ? b : a;
                    for (int j = 0; j < n; j++) {
                        Node x = (Node) from[j];
                        to[j] = x;
                        from[j] = null;
                        seed = seed * 6364136223846793005L + 1442695040888963407L;
                        int k = (int) ((seed >>> 33) % n);
                        Node y = (Node) to[k];
                        if (y == null) y = (Node) from[k];
                        x.next = y;
                        // Replace one node in 64 with a fresh copy (born during the
                        // phase), dropping the old one.
                        if ((j & 63) == 0) {
                            Node fresh = new Node(x.id);
                            fresh.next = x.next;
                            to[j] = fresh;
                        }
                        if ((j & 7) == 0) junk[jk++ & 255] = new long[16];
                    }
                    if (large && (r % 4) == 0) junk[jk++ & 255] = new long[64 * 1024];
                }
                Object[] last = (rounds & 1) == 0 ? a : b;
                Object[] other = (rounds & 1) == 0 ? b : a;
                boolean[] seen = new boolean[n];
                for (int j = 0; j < n; j++) {
                    if (other[j] != null) corrupt[me]++;
                    Node x = (Node) last[j];
                    if (x == null) continue; // counted as lost below
                    if (x.id != j || !x.intact()) {
                        corrupt[me]++;
                        continue;
                    }
                    seen[j] = true;
                    Node y = x.next;
                    if (y != null && (y.id < 0 || y.id >= n || !y.intact())) corrupt[me]++;
                }
                for (int j = 0; j < n; j++) if (!seen[j]) lost[me]++;
            });
            ts[t].start();
        }
        for (Thread t : ts) t.join();
        int l = 0, c = 0;
        for (int t = 0; t < threads; t++) {
            l += lost[t];
            c += corrupt[t];
        }
        System.out.println("ZR3CS-EVERY threads=" + threads + " rounds=" + rounds + " live=" + liveMiB
                + " large=" + large + " lost=" + l + " corrupt=" + c + (l == 0 && c == 0 ? " ok" : " BAD"));
    }
}
