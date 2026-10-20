// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 69, lane L2 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i67-L2-proposal-worklist-liveness-fixed-point-KEPT-OFF-20261031.md.
//
// Switch, opt-in:
//   CRATONVM_JIT_IR_LIVENESS_WORKLIST=1  `regalloc::liveness_core`'s backward
//       fixed point visits, in each pass, only the blocks a successor's
//       live-in change made dirty (same pass order as the round robin). A
//       pure refactor: the liveness answer, converged or cut off, is the
//       round-robin one, so frames, allocations and timings of the compiled
//       code must be IDENTICAL with it on and off. It reaches the JIT only
//       where `liveness_core` runs: the live model under
//       CRATONVM_JIT_IR_SHARED_LIVENESS_CORE=1, the slot plan's core path,
//       and CRATONVM_JIT_IR_ONE_LIVENESS_PASS=1.
//
// The real measurement is the Rust bench (ns per graph, block visits):
//   cargo test -p cratonvm-jit --release --lib i69_l2_bench -- --ignored --nocapture
//
// Checks (stderr), on the methods below (`CRATONVM_DBG_IR_COMPILES=1` names
// them):
//   1. Byte identity: with CRATONVM_JIT_IR_SHARED_LIVENESS_CORE=1
//      CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES=1 CRATONVM_JIT_IR_ONE_LIVENESS_PASS=1
//      CRATONVM_DBG_IR_SLOTS=1 CRATONVM_DBG_IR_LINEAR_SCAN=1, run with the
//      worklist switch unset and `=1`; the `[ir-slots]` and
//      `[ir] linear scan:` lines must diff empty.
//   2. Compile time: the per-compile timings of `CRATONVM_DBG_IR_COMPILES=1`
//      for `sequentialLoops` (many back edges, the expected win), `nest`
//      (a depth-4 loop nest) and `switchy` (a 16-way tableswitch in a loop),
//      switch unset vs `=1`.
//
// Rows (stderr, min ns per inner iteration over REPS=9 calls; JIT on). They
// are steady-state code, which the switch must not change: the regression
// guard, not the expected win. A/B: one binary, interleaved runs, medians of
// the per-run minimums.
//
// stdout carries only checksums, identical on every VM and every switch:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// sequentialLoops checksum=179998200000
// nest checksum=199680000
// switchy checksum=13600000
// HOTSPOT_EXPECTED_END
public class L2W69LivenessWorklistBench {
    static final int REPS = 9;
    static final int SEQ_N = 100000;
    static final int NEST_N = 40;
    static final int SWITCH_N = 1600000;

    static long sequentialLoops(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        for (int i = 0; i < n; i++) {
            s += 2L * i;
        }
        for (int i = 0; i < n; i++) {
            s += 3L * i;
        }
        for (int i = 0; i < n; i++) {
            s += 4L * i;
        }
        for (int i = 0; i < n; i++) {
            s += 5L * i;
        }
        for (int i = 0; i < n; i++) {
            s += 6L * i;
        }
        for (int i = 0; i < n; i++) {
            s += 7L * i;
        }
        for (int i = 0; i < n; i++) {
            s += 8L * i;
        }
        return s;
    }

    static long nest(int n) {
        long s = 0;
        for (int a = 0; a < n; a++) {
            for (int b = 0; b < n; b++) {
                for (int c = 0; c < n; c++) {
                    for (int d = 0; d < n; d++) {
                        s += a + b + c + d;
                    }
                }
            }
        }
        return s;
    }

    static long switchy(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            switch (i & 15) {
                case 0: s += 1; break;
                case 1: s += 2; break;
                case 2: s += 3; break;
                case 3: s += 4; break;
                case 4: s += 5; break;
                case 5: s += 6; break;
                case 6: s += 7; break;
                case 7: s += 8; break;
                case 8: s += 9; break;
                case 9: s += 10; break;
                case 10: s += 11; break;
                case 11: s += 12; break;
                case 12: s += 13; break;
                case 13: s += 14; break;
                case 14: s += 15; break;
                default: s += 16; break;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        long best = Long.MAX_VALUE;
        long sum = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = sequentialLoops(SEQ_N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("sequentialLoops checksum=" + sum);
        System.err.printf("sequentialLoops %.3f ns/op%n", best / (double) (8L * SEQ_N));

        best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = nest(NEST_N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("nest checksum=" + sum);
        long nestOps = (long) NEST_N * NEST_N * NEST_N * NEST_N;
        System.err.printf("nest            %.3f ns/op%n", best / (double) nestOps);

        best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = switchy(SWITCH_N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("switchy checksum=" + sum);
        System.err.printf("switchy         %.3f ns/op%n", best / (double) SWITCH_N);
    }
}
