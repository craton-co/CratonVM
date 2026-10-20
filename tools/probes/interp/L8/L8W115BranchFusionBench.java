// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i6 wave 115, lane L8: the two branch fusions, each
// default off.
//
//   CRATONVM_INTERP_FUSED_ZERO_BRANCH  `iload{_N, n}; if<cond>` (the wave-88
//     census's `iload_1 -> ifne`, in 97 of 169 bench probes)
//   CRATONVM_INTERP_FUSED_NULL_BRANCH  `aload_N; ifnull | ifnonnull`
//
// Rows (all locals in 0..3 except `wideRow`):
//   whileRow   while (n != 0) { s += n & 7; n--; }   iload_0; ifeq (forward)  ZERO
//   doRow      do { ...; n--; } while (n != 0)       iload_0; ifne (back edge) ZERO
//   bitRow     int b = i & 1; if (b != 0) s++        iload_3; ifeq            ZERO
//   wideRow    the bitRow loop with b in local 6     iload 6; ifeq            ZERO
//   walkRow    for (n = head; n != null; n = n.next) aload_3; ifnull          NULL
//   doWalkRow  do { ... } while (n != null)          aload_2; ifnonnull       NULL
//   slotRow    Object o = a[i]; if (o != null) s++   aload_3; ifnull          NULL
//   control    s += i                                 already fused; flat with both
// The ZERO rows should drop with CRATONVM_INTERP_FUSED_ZERO_BRANCH=1 and stay
// flat with the NULL switch alone; the NULL rows the other way round;
// `control` flat in every arm (the off cost is one byte test per `iload` /
// `aload_N` that reaches the test).
//
// Every row runs REPS timed repetitions after one untimed warm-up and
// reports the MINIMUM. Compare arms on the SAME binary, interleaved, medians
// of the per-round minima. On the Windows dev box pin to the P-cores
// (logical CPUs 0-15), or an E-core run reads 1.8-2.8x slower
// (docs/known-issues/interpreter/gap-hybrid-cpu-e-core-scheduling-halves-interpreter-timings-20261018.md):
//   start /affinity FFFF /b /wait cratonvm --java-home <jdk25> --nojit -cp <dir> L8W115BranchFusionBench
//
// Run (arms):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L8W115BranchFusionBench
//   CRATONVM_INTERP_FUSED_ZERO_BRANCH=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L8W115BranchFusionBench
//   CRATONVM_INTERP_FUSED_NULL_BRANCH=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L8W115BranchFusionBench
//   (both switches) and once without --nojit (JIT arm: expected flat)
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op` line (not compared). About 2 s under --nojit.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// whileRow checksum=1750000
// doRow checksum=1750000
// bitRow checksum=250000
// wideRow checksum=250000
// walkRow checksum=124999750000
// doWalkRow checksum=124999750000
// slotRow checksum=166667
// control checksum=124999750000
// HOTSPOT_EXPECTED_END
public class L8W115BranchFusionBench {
    interface Row {
        long run();
    }

    static final int REPS = 5;
    static final int ITERS = 500_000;

    static final class Node {
        final int v;
        Node next;

        Node(int v) {
            this.v = v;
        }
    }

    static Node head;
    static Object[] slots;

    static long whileRow() {
        int n = ITERS;
        long s = 0;
        while (n != 0) {
            s += n & 7;
            n--;
        }
        return s;
    }

    static long doRow() {
        int n = ITERS;
        long s = 0;
        do {
            s += n & 7;
            n--;
        } while (n != 0);
        return s;
    }

    static long bitRow() {
        long s = 0;
        for (int i = 0; i < ITERS; i++) {
            int b = i & 1;
            if (b != 0) {
                s++;
            }
        }
        return s;
    }

    // Two long parameters fill locals 0-3, so `s` is 4, `i` 5 and `b` 6:
    // `iload 6; ifeq`.
    static long wideRow(long pad0, long pad1) {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            int b = i & 1;
            if (b != 0) {
                s++;
            }
        }
        return s + pad0 + pad1;
    }

    static long walkRow() {
        long s = 0;
        Node h = head;
        for (Node n = h; n != null; n = n.next) {
            s += n.v;
        }
        return s;
    }

    static long doWalkRow() {
        long s = 0;
        Node n = head;
        do {
            s += n.v;
            n = n.next;
        } while (n != null);
        return s;
    }

    // `a` 0, `s` 1 (an int, so `o` is local 3), `i` 2, `o` 3.
    static long slotRow() {
        Object[] a = slots;
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            Object o = a[i];
            if (o != null) {
                s++;
            }
        }
        return s;
    }

    static long control() {
        long s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i;
        }
        return s;
    }

    static void time(String name, Row row) {
        long checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf(java.util.Locale.ROOT, "TIME %-10s %8.2f ns/op (min of %d)%n", name, (double) best / ITERS, REPS);
    }

    public static void main(String[] args) {
        Node h = null;
        for (int i = ITERS - 1; i >= 0; i--) {
            Node n = new Node(i);
            n.next = h;
            h = n;
        }
        head = h;
        slots = new Object[ITERS];
        for (int i = 0; i < ITERS; i += 3) {
            slots[i] = Integer.valueOf(i);
        }
        time("whileRow", L8W115BranchFusionBench::whileRow);
        time("doRow", L8W115BranchFusionBench::doRow);
        time("bitRow", L8W115BranchFusionBench::bitRow);
        time("wideRow", () -> wideRow(0L, 0L));
        time("walkRow", L8W115BranchFusionBench::walkRow);
        time("doWalkRow", L8W115BranchFusionBench::doWalkRow);
        time("slotRow", L8W115BranchFusionBench::slotRow);
        time("control", L8W115BranchFusionBench::control);
    }
}
