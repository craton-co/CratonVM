// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gch h3/fd (2026-10-08,
 * {@code gcd-d2g-proposal-tag-root-occurrences-by-rewritability-20260927}):
 * young objects whose only DIRECT holders are interpreted locals of a
 * long-lived frame ({@code main}'s), while compiled code allocates.
 *
 * <p>Under a live JIT the Generational young collection may take the
 * non-moving sweep, whose selective promotion pins every young ROOT value. A
 * value only {@code main}'s locals name is then pinned in young for as long as
 * {@code main} runs. {@code CRATONVM_GEN_PRECISE_ROOT_PROMOTE_FRAMES=1} (with
 * {@code CRATONVM_GEN_PRECISE_ROOT_PROMOTE=1}) counts those locals as
 * rewritten occurrences, so the sweep may promote them; the
 * {@code [GC] precise_root_promote: prp_exempt=} line ({@code CRATONVM_DBG=gc-stats})
 * is the positive control. {@code main} itself runs no long loop (no OSR):
 * the lists are built and checked by callees, and the churn is a separate
 * method the JIT compiles.
 *
 * <p>Every list is re-walked after the churn, so a holder the collector failed
 * to rewrite (a promoted object left at its old address in a local) shows as
 * a wrong checksum or a crash.
 *
 * <p>STDOUT (deterministic, identical on HotSpot 25):
 * <pre>
 *   local-held-young sum=... lists=... PASS
 * </pre>
 */
public class GchH3fdLocalHeldYoungProbe {
    static final class Node {
        Node next;
        final int v;
        final byte[] pad = new byte[40];

        Node(int v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    static Node build(int n, int salt) {
        Node head = null;
        for (int i = 0; i < n; i++) {
            head = new Node(i ^ salt, head);
        }
        return head;
    }

    static long walk(Node head) {
        long s = 0;
        for (Node n = head; n != null; n = n.next) {
            s += n.v;
        }
        return s;
    }

    /** Young garbage, compiled after the first rounds. */
    static long churn(int r) {
        long s = 0;
        for (int i = 0; i < 150_000; i++) {
            byte[] b = new byte[24 + ((i + r) & 31)];
            s += b.length;
        }
        return s;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        Node a = build(4096, 1);
        Node b = build(4096, 2);
        Node c = build(4096, 3);
        Object[] holder = new Object[] {build(1024, 4), build(1024, 5)};
        long expect = walk(a) + walk(b) + walk(c) + walk((Node) holder[0]) + walk((Node) holder[1]);
        long sum = 0;
        long lists = 0;
        boolean ok = true;
        for (int r = 0; r < rounds; r++) {
            sum += churn(r);
            long now = walk(a) + walk(b) + walk(c) + walk((Node) holder[0]) + walk((Node) holder[1]);
            ok &= now == expect;
            lists = now;
            if ((r & 7) == 7) {
                // A fresh young list into a held local, mid-run.
                c = build(4096, 3);
            }
        }
        System.out.println("local-held-young sum=" + sum + " lists=" + lists + (ok ? " PASS" : " FAIL"));
    }
}
