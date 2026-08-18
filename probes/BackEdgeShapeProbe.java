// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Back-edge SHAPE probe (interpreter audit 2026-08-18).
//
// Before this audit the interpreter's back-edge accounting — bump
// `Frame::backward_count`, record a PGO back edge, offer the frame to
// `try_osr_with_backoff` — lived only in the raw-bytecode fast path, and only
// on `goto` (0xa7) and the twelve int-comparison branches. A loop whose back
// edge is `ifnonnull` / `ifnull` / `if_acmpeq` / `if_acmpne` reached the
// decoded handler in `opcodes.rs`, which sets `frame.pc` and nothing else, so
// such a loop:
//
//   * never reached the OSR threshold, and
//   * never earned whole-method tier-up credit either, because
//     `pop_and_recycle_frame_with_reason` feeds `ProfileStore::add_loop_work`
//     from that same counter.
//
// javac puts a `while`/`for` test at the TOP of the loop and closes it with a
// `goto`, so those shapes were always accounted. The shape that was not is
// `do { … } while (ref-condition)` — which is what this probe measures, and
// which is also what a bottom-test frontend (ECJ, the Kotlin and Scala
// backends) emits for ordinary `while` loops.
//
// Both arms below do identical work on identical data. `intEdge` is the
// CONTROL: `do { … } while (i < n)` closes on `if_icmplt`, an accounted site.
// `refEdge` is the arm under test: `do { … } while (p != null)` closes on
// `ifnonnull`. Read the wall-clock ratio TOGETHER with `--jit-stats`: the
// number that moves is not only the time, it is whether the loop is visible to
// tier-up at all.
public final class BackEdgeShapeProbe {

    static final class Node {
        Node next;
        int v;
    }

    private static Node chain(int n) {
        Node head = null;
        for (int i = n - 1; i >= 0; i--) {
            Node t = new Node();
            t.v = i & 0xff;
            t.next = head;
            head = t;
        }
        return head;
    }

    // CONTROL. Identical walk over the identical chain, but javac puts the
    // test at the TOP and closes the loop with `goto` (0xa7) — an accounted
    // site, before and after this audit.
    private static long gotoEdge(Node head) {
        long s = 0;
        Node p = head;
        while (p != null) {
            s += p.v;
            p = p.next;
        }
        return s;
    }

    // ARM UNDER TEST. Same chain, same field reads, same arithmetic. The ONLY
    // difference is the closing opcode: `do { … } while` puts the test at the
    // BOTTOM, so the back edge is `ifnonnull` (0xc7) — unaccounted before this
    // audit. Anything that separates these two numbers is the edge shape,
    // because nothing else about the two loops differs.
    private static long condEdge(Node head) {
        long s = 0;
        Node p = head;
        do {
            s += p.v;
            p = p.next;
        } while (p != null);
        return s;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 1_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 20;

        // ONE chain, walked by both arms, so cache behaviour is not a variable.
        Node head = chain(n);

        // Interleave the arms so a host hiccup cannot land on one of them.
        long tg = 0, tc = 0, sg = 0, sc = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            sg += gotoEdge(head);
            long t1 = System.nanoTime();
            sc += condEdge(head);
            long t2 = System.nanoTime();
            tg += t1 - t0;
            tc += t2 - t1;
        }
        if (sg != sc) {
            throw new IllegalStateException("arms disagree: " + sg + " vs " + sc);
        }
        System.out.println("checksum=" + sg);
        System.out.println("gotoEdge(goto back edge)      total_ms=" + (tg / 1_000_000));
        System.out.println("condEdge(ifnonnull back edge) total_ms=" + (tc / 1_000_000));
        System.out.println("ratio cond/goto=" + ((double) tc / (double) tg));
    }
}
