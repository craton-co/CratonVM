// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 83, lane L7: ns per element for
//   CRATONVM_INTERP_FUSED_WIDE_LOCAL_GETFIELD  the `aload n` arm fuses
//       `aload n; getfield` (a receiver past local 3) through the
//       out-of-line quickened read
//       (interpreter-i72-L7-proposal-wide-local-forms-of-the-fused-groups-KEPT-OFF-20261019;
//       default off)
//
// Rows (CALLS = 1000 calls over N = 1000 elements each):
//   wideWalk   a linked-list walk whose cursor is local 7 (two leading `long`
//              parameters): `aload 7; getfield i` and `aload 7; getfield
//              next` per node. On: two dispatches fewer per node, minus a
//              call each; expected down.
//   wideRead   `s += n.i` with `n` in local 4. On: expected down.
//   shortWalk  the same walk with the cursor in local 3 (`aload_3;
//              getfield`, fused since round i1). Control: flat.
//   wideArray  `s += a.length` with `a` in local 4 (`aload 4;
//              arraylength`, fused since wave 5): the `aload n` arm's other
//              pair. Control: flat (the off cost is one byte compare).
//
// Robust to a loaded host: every row runs REPS = 9 timed repetitions after
// one untimed warm-up and reports the MINIMUM. Compare the switch off / on
// on the SAME binary, interleaved, several rounds, and take the median of
// the per-round minima.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W83WideLocalGetfieldBench
// stdout: per row one deterministic `<row> checksum=<n>` line (identical on
// HotSpot 25 and under every switch) and one `<row> <x> ns/op (min of N)`
// timing line.
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// wideWalk checksum=500500000
// wideRead checksum=7000000
// shortWalk checksum=500500000
// wideArray checksum=1000000
// HOTSPOT_EXPECTED_END
// (the timing lines, which contain "ns/", are not compared)
public class L7W83WideLocalGetfieldBench {
    interface Row {
        long run();
    }

    static final int REPS = 9;
    static final int CALLS = 1000;
    static final int N = 1000;

    static final class Node {
        int i;
        Node next;
    }

    // head 4, s 5..6, n 7.
    static long wideWalk(long pad0, long pad1, Node head) {
        long s = 0;
        for (Node n = head; n != null; n = n.next) {
            s += n.i;
        }
        return s;
    }

    // n 4, iters 5, s 6..7, k 8.
    static long wideRead(long pad0, long pad1, Node n, int iters) {
        long s = 0;
        for (int k = 0; k < iters; k++) {
            s += n.i;
        }
        return s;
    }

    // head 0, s 1..2, n 3.
    static long shortWalk(Node head) {
        long s = 0;
        for (Node n = head; n != null; n = n.next) {
            s += n.i;
        }
        return s;
    }

    // a 4, iters 5, s 6..7, k 8.
    static long wideArray(long pad0, long pad1, int[] a, int iters) {
        long s = 0;
        for (int k = 0; k < iters; k++) {
            s += a.length;
        }
        return s;
    }

    static void row(String name, Row r) {
        long checksum = r.run(); // untimed warm-up: fills the sites
        long best = Long.MAX_VALUE;
        for (int rep = 0; rep < REPS; rep++) {
            long t0 = System.nanoTime();
            long c = r.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%-11s %8.2f ns/op (min of %d)%n", name, (double) best / ((long) CALLS * N), REPS);
    }

    public static void main(String[] args) {
        Node head = null;
        for (int v = N; v >= 1; v--) {
            Node x = new Node();
            x.i = v;
            x.next = head;
            head = x;
        }
        final Node list = head;
        final Node one = new Node();
        one.i = 7;
        final int[] arr = new int[1];
        row("wideWalk", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += wideWalk(0, 0, list);
            }
            return t;
        });
        row("wideRead", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += wideRead(0, 0, one, N);
            }
            return t;
        });
        row("shortWalk", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += shortWalk(list);
            }
            return t;
        });
        row("wideArray", () -> {
            long t = 0;
            for (int c = 0; c < CALLS; c++) {
                t += wideArray(0, 0, arr, N);
            }
            return t;
        });
    }
}
