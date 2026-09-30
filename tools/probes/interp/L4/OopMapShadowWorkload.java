// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 7, lane L4: a small workload for the precise
// interpreter oop-map shadow (docs/known-issues/interpreter/
// i1-L4-proposal-precise-interpreter-oop-maps-20260923.md, stage 1). It
// allocates enough to collect many times while interpreter frames hold the
// shapes the shadow compares: a scoped-out reference temp that is dead but
// still in its slot, `long`/`double` locals beside references (one of them
// pointer-shaped), a reference on the operand stack across an allocating
// call (`new Node(build(d - 1), d)`: the frame is stopped inside the invoke,
// so its stack is between two map rows), and an exception handler that reads
// a local the try body never touches.
//
// Correctness run (the probe runner): stdout must equal HotSpot's, with and
// without --nojit. Expected on HotSpot 25 (three identical rounds):
//   round 0: scoped=499740 longs=7fff000000bf6868 tree=1005050 handler=1556
//   round 1: scoped=499740 longs=7fff000000bf6868 tree=1005050 handler=1556
//   round 2: scoped=499740 longs=7fff000000bf6868 tree=1005050 handler=1556
//
// Measurement run (extra setup, stdout unchanged): run it on CratonVM with
//   CRATONVM_DBG_VERIFY_OOP_MAPS=1 --nojit -Xmx32m
// and read the `[interp-oopmap-summary]` line printed at exit. `frames` must be
// non-zero (engagement); `missed` / `remap_missed` name references the
// heuristic scan or remap did not cover (each is a finding); `extra` /
// `remap_extra` are the over-retention stage 3 would remove.
public class OopMapShadowWorkload {
    static final class Node {
        final Node next;
        final int v;

        Node(Node next, int v) {
            this.next = next;
            this.v = v;
        }
    }

    // A list built and summed in an inner scope; `head` is dead afterwards
    // but stays in its slot while the churn loop collects.
    static int scopedTemps(int n) {
        int sum = 0;
        {
            Node head = null;
            for (int i = 0; i < n; i++) {
                head = new Node(head, i);
            }
            for (Node p = head; p != null; p = p.next) {
                sum += p.v;
            }
        }
        long churn = 0;
        for (int i = 0; i < n * 20; i++) {
            int[] a = new int[16];
            a[i & 15] = i;
            churn += a[i & 15];
        }
        return sum + (int) (churn & 0xff);
    }

    // Category-2 primitives next to references; `acc` starts pointer-shaped.
    static long longsAndRefs(int n) {
        long acc = 0x7fff_0000_0000_1000L;
        Object keep = new int[] {7};
        double d = 1.5;
        for (int i = 0; i < n; i++) {
            Object tmp = new long[8];
            acc += ((long[]) tmp).length + i;
            d += 0.5;
        }
        return acc + ((int[]) keep)[0] + (long) d;
    }

    // `new Node` + `dup` sit on the operand stack across the recursive call.
    static Node build(int depth) {
        if (depth == 0) {
            return new Node(null, 1);
        }
        return new Node(build(depth - 1), depth);
    }

    static int tree(int reps) {
        int s = 0;
        for (int r = 0; r < reps; r++) {
            Node n = build(200);
            for (Node p = n; p != null; p = p.next) {
                s += p.v;
            }
        }
        return s;
    }

    // `held` is read only by the handler.
    static int handler(int n) {
        int caught = 0;
        for (int i = 0; i < n; i++) {
            Object held = new StringBuilder().append(i);
            try {
                if (i % 7 == 0) {
                    throw new IllegalStateException("x");
                }
                byte[] b = new byte[64];
                caught += b.length & 1;
            } catch (IllegalStateException e) {
                caught += held.toString().length();
            }
        }
        return caught;
    }

    public static void main(String[] args) {
        for (int round = 0; round < 3; round++) {
            int scoped = scopedTemps(1000);
            long longs = longsAndRefs(5000);
            int t = tree(50);
            int h = handler(3000);
            System.out.println("round " + round + ": scoped=" + scoped + " longs="
                    + Long.toHexString(longs) + " tree=" + t + " handler=" + h);
        }
    }
}
