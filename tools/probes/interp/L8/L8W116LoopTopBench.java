// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i6 wave 116, lane L8: three switches, each default off.
//
//   CRATONVM_INTERP_LEAN_LOOP_TOP           the dispatch loop's lean loop-top
//     instance: one gate test per bytecode instead of three, and the code
//     length cached at the frame-switch compare (every row; chosen once per
//     interpreter entry)
//   CRATONVM_INTERP_FUSED_GETFIELD_CHAIN    `getfield; getfield [; ...]` after
//     a quickened hit, in the same dispatch
//   CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE  `aload n; ifnull | ifnonnull`
//
// Rows:
//   straight     int arithmetic, no field, no call      LEAN
//   callRow      s += id(i): a frame switch per call     LEAN (refresh path)
//   chainRow     s += h.a.b.c, h in local 1              CHAIN (fusion head)
//   staticChain  s += S.a.b.c                            CHAIN (getfield-arm head)
//   wideWalk     list walk, cursor in local 5            NULL_WIDE
//   control      s += i                                  LEAN only
// With LEAN every row should be flat or lower (straight and control most);
// CHAIN lowers chainRow and staticChain only; NULL_WIDE lowers wideWalk only.
//
// Every row runs REPS timed repetitions after one untimed warm-up and
// reports the MINIMUM. Compare arms on the SAME binary, interleaved, medians
// of the per-round minima. On the Windows dev box pin to the P-cores
// (logical CPUs 0-15), or an E-core run reads 1.8-2.8x slower
// (docs/known-issues/interpreter/gap-hybrid-cpu-e-core-scheduling-halves-interpreter-timings-20261018.md):
//   start /affinity FFFF /b /wait cratonvm --java-home <jdk25> --nojit -cp <dir> L8W116LoopTopBench
//
// JIT rows (wave 118): pass a scale, e.g. 250, so each TIME line is tens of
// milliseconds (`cratonvm --java-home <jdk25> -cp <dir> L8W116LoopTopBench 250`).
// Every switch here changes only the interpreter's dispatch loop; compiled code
// is the same with each switch on or off (see the wave-118 note on
// i1-L1-proposal-dispatch-loop-register-state).
//
// Run (arms):
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L8W116LoopTopBench
//   CRATONVM_INTERP_LEAN_LOOP_TOP=1 cratonvm ... L8W116LoopTopBench
//   CRATONVM_INTERP_FUSED_GETFIELD_CHAIN=1 cratonvm ... L8W116LoopTopBench
//   CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE=1 cratonvm ... L8W116LoopTopBench
//   (all three) and once without --nojit (JIT arm: expected flat)
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `TIME <row> <x> ns/op` line (not compared). About 2-3 s under --nojit.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// straight checksum=-62645171
// callRow checksum=79999800000
// chainRow checksum=1200000
// staticChain checksum=1200000
// wideWalk checksum=79999800000
// control checksum=79999800000
// HOTSPOT_EXPECTED_END
public class L8W116LoopTopBench {
    interface Row {
        long run();
    }

    static final int REPS = 5;
    static final int ITERS = 400_000;
    // Wave 118 (lane L8): `L8W116LoopTopBench <scale>` runs every row `scale`
    // times longer (the list walk walks the same list `scale` times), so a
    // JIT run reads in microseconds per TIME line rather than sub-ns noise.
    // The checksums in the HotSpot block are for scale 1.
    static int scale = 1;
    static int loops = ITERS;

    static final class Leaf {
        int c;
    }

    static final class Mid {
        Leaf b;
    }

    static final class Head {
        Mid a;
    }

    static final class Node {
        final int v;
        Node next;

        Node(int v) {
            this.v = v;
        }
    }

    static Head shared;
    static Node list;

    static long straight() {
        int s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            s = s * 31 + i;
            s ^= s >>> 7;
        }
        return s;
    }

    static int id(int x) {
        return x;
    }

    static long callRow() {
        long s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            s += id(i);
        }
        return s;
    }

    static long chainRow() {
        int s = 0;
        Head h = shared;
        int n = loops;
        for (int i = 0; i < n; i++) {
            s += h.a.b.c;
        }
        return s;
    }

    static long staticChain() {
        int s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
            s += shared.a.b.c;
        }
        return s;
    }

    // Two long parameters fill locals 0-3: `s` is 4-5 and `n` is 6, so the
    // loop test is `aload 6; ifnull`.
    static long wideWalkOnce(long p0, long p1) {
        long s = p0 + p1;
        for (Node n = list; n != null; n = n.next) {
            s += n.v;
        }
        return s;
    }

    static long wideWalk() {
        long s = 0;
        for (int r = 0; r < scale; r++) {
            s += wideWalkOnce(0L, 0L);
        }
        return s;
    }

    static long control() {
        long s = 0;
        int n = loops;
        for (int i = 0; i < n; i++) {
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
        System.out.printf(java.util.Locale.ROOT, "TIME %-11s %8.2f ns/op %10.1f us (min of %d, scale %d)%n",
                name, (double) best / loops, best / 1000.0, REPS, scale);
    }

    public static void main(String[] args) {
        if (args.length > 0) {
            scale = Integer.parseInt(args[0]);
            loops = ITERS * scale;
        }
        Leaf l = new Leaf();
        l.c = 3;
        Mid m = new Mid();
        m.b = l;
        Head h = new Head();
        h.a = m;
        shared = h;
        Node head = null;
        for (int i = ITERS - 1; i >= 0; i--) {
            Node n = new Node(i);
            n.next = head;
            head = n;
        }
        list = head;
        time("straight", L8W116LoopTopBench::straight);
        time("callRow", L8W116LoopTopBench::callRow);
        time("chainRow", L8W116LoopTopBench::chainRow);
        time("staticChain", L8W116LoopTopBench::staticChain);
        time("wideWalk", L8W116LoopTopBench::wideWalk);
        time("control", L8W116LoopTopBench::control);
    }
}
