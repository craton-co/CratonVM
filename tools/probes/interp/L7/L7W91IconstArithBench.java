// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 91, lane L7: CRATONVM_INTERP_FUSED_ICONST_ARITH
// (default off), the `iload_N` arm's `iload_N; {iconst_m1..5 | bipush |
// sipush}; <int op> [; istore_Z]` as one dispatch, picked from the wave-88
// opcode-pair census (`iconst_1 -> iadd` in 56 of 169 bench probes,
// `bipush -> iand` in 34;
// docs/internal/fixed-bugs/interpreter-i85-L7-proposal-opcode-pair-census-of-the-battery-DONE-20261122.md).
//
// Rows (every local in 0..3, so the group starts at an iload_N):
//   maskLoop     s += i & 0xff      iload_1; sipush 255; iand (push form)
//   storeLoop    int j = i + 1;     iload_1; iconst_1; iadd; istore_2 (store form)
//                s ^= j
//   shiftLoop    s += i >>> 3       iload_1; iconst_3; iushr
//   mulLoop      s += i * 31        iload_1; bipush 31; imul
//   fibRow       fib(24)            n - 1 / n - 2 ahead of an invokestatic
//   controlLoop  s += i             already fused (iload; iload; iadd; istore): flat
// maskLoop .. fibRow should drop with the switch on (one dispatch fewer per
// group, two for the store form); controlLoop is the off cost of the
// arm's extra byte test (expected flat).
//
// Every row runs REPS = 9 timed repetitions after one untimed warm-up and
// reports the MINIMUM; compare on the SAME binary, interleaved, medians of
// the per-round minima.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W91IconstArithBench
//   CRATONVM_INTERP_FUSED_ICONST_ARITH=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W91IconstArithBench
// stdout: per row one deterministic `<row> checksum=<n>` line and one
// `<row> <x> ns/op (min of N)` timing line (not compared).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// maskLoop checksum=254991808
// storeLoop checksum=2000000
// shiftLoop checksum=890896832
// mulLoop checksum=2116082240
// fibRow checksum=46368
// controlLoop checksum=-1455759936
// HOTSPOT_EXPECTED_END
public class L7W91IconstArithBench {
    interface Row {
        int run();
    }

    static final int REPS = 9;
    static final int ITERS = 2_000_000;
    // fib(24) makes 2 * fib(25) - 1 calls.
    static final int FIB_CALLS = 150_049;

    static int maskLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i & 0xff;
        }
        return s;
    }

    static int storeLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            int j = i + 1;
            s ^= j;
        }
        return s;
    }

    static int shiftLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i >>> 3;
        }
        return s;
    }

    static int mulLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i * 31;
        }
        return s;
    }

    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    static int fibRow() {
        return fib(24);
    }

    static int controlLoop() {
        int s = 0;
        for (int i = 0; i < ITERS; i++) {
            s += i;
        }
        return s;
    }

    static void time(String name, Row row, int ops) {
        int checksum = row.run(); // untimed warm-up
        long best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            int c = row.run();
            long dt = System.nanoTime() - t0;
            if (dt < best) {
                best = dt;
            }
            if (c != checksum) {
                checksum = -1;
            }
        }
        System.out.println(name + " checksum=" + checksum);
        System.out.printf("%-15s %8.2f ns/op (min of %d)%n", name, (double) best / ops, REPS);
    }

    public static void main(String[] args) {
        time("maskLoop", L7W91IconstArithBench::maskLoop, ITERS);
        time("storeLoop", L7W91IconstArithBench::storeLoop, ITERS);
        time("shiftLoop", L7W91IconstArithBench::shiftLoop, ITERS);
        time("mulLoop", L7W91IconstArithBench::mulLoop, ITERS);
        time("fibRow", L7W91IconstArithBench::fibRow, FIB_CALLS);
        time("controlLoop", L7W91IconstArithBench::controlLoop, ITERS);
    }
}
