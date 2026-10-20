// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L2 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i65-L2-proposal-one-liveness-pass-per-compile-KEPT-OFF-20261029.md.
//
// Switch, opt-in:
//   CRATONVM_JIT_IR_ONE_LIVENESS_PASS=1  the optimizing lowering entry runs
//       `regalloc::liveness_core` once and hands it to both the slot plan and
//       the allocator's live model (residency, OSR entry stubs). It applies
//       only when both consumers ask the same question, which in practice
//       needs CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES=1 as well (or
//       CRATONVM_JIT_IR_PHI_EDGE_INTERFERE=0). A pure saving: frames,
//       allocations and timings must be IDENTICAL with it on and off.
//
// Checks (stderr), on the methods below (`CRATONVM_DBG_IR_COMPILES=1` names
// them):
//   1. Engagement: with `CRATONVM_JIT_IR_ONE_LIVENESS_PASS=1
//      CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES=1 CRATONVM_DBG_IR_LINEAR_SCAN=1`
//      every IR compile prints `[ir-ls] one liveness pass: shared`. Without
//      the phi switch it prints `rules-differ` (today's default rules).
//   2. Byte identity: with CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES=1
//      CRATONVM_DBG_IR_SLOTS=1 CRATONVM_DBG_IR_LINEAR_SCAN=1, run with the
//      one-pass switch unset and `=1`; after dropping the
//      `[ir-ls] one liveness pass:` lines, the `[ir-slots]` and
//      `[ir] linear scan:` lines must diff empty.
//   3. Compile time: the per-compile timings of `CRATONVM_DBG_IR_COMPILES=1`
//      for `manyPhis` and `branchy` (the largest graphs here), switch unset vs
//      `=1`. Expect a small saving, growing with nodes x blocks.
//
// Rows (stderr, min ns per inner iteration over REPS=9 calls; JIT on):
//   sumLoop    `for (i) s += i`: one counter phi, one accumulator phi;
//   manyPhis   eight long accumulators: nine loop phis, a wide live set;
//   branchy    a four-way `i % 4` chain in the loop body: many blocks;
//   callLoop   `s += next(i)`: a phi live across a call in the loop body.
// A/B: the switch unset vs `=1`, one binary, interleaved runs, medians of the
// per-run minimums (in-JVM timings swing ~3x on a noisy host). The rows are
// steady-state code, which the switch must not change; they are the
// regression guard, not the expected win.
//
// stdout carries only checksums, identical on every VM and every switch:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// sumLoop checksum=499999500000
// manyPhis checksum=4000024000000
// branchy checksum=187500000000
// callLoop checksum=14999950000
// HOTSPOT_EXPECTED_END
public class L2W66OneLivenessPassBench {
    static final int REPS = 9;
    static final int N = 1000000;
    static final int CALLS = 100000;

    static long sumLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long manyPhis(int n) {
        long a0 = 0, a1 = 0, a2 = 0, a3 = 0, a4 = 0, a5 = 0, a6 = 0, a7 = 0;
        for (int i = 0; i < n; i++) {
            a0 += i;
            a1 += i + 1;
            a2 += i + 2;
            a3 += i + 3;
            a4 += i + 4;
            a5 += i + 5;
            a6 += i + 6;
            a7 += i + 7;
        }
        return a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7;
    }

    static long branchy(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int r = i % 4;
            if (r == 0) {
                s += i;
            } else if (r == 1) {
                s -= 1;
            } else if (r == 2) {
                s += i >> 1;
            } else {
                s += 3;
            }
        }
        return s;
    }

    static int next(int i) {
        return i * 3 + 1;
    }

    static long callLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += next(i);
        }
        return s;
    }

    public static void main(String[] args) {
        long best = Long.MAX_VALUE;
        long sum = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = sumLoop(N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("sumLoop checksum=" + sum);
        System.err.printf("sumLoop    %.3f ns/op%n", best / (double) N);

        best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = manyPhis(N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("manyPhis checksum=" + sum);
        System.err.printf("manyPhis   %.3f ns/op%n", best / (double) N);

        best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = branchy(N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("branchy checksum=" + sum);
        System.err.printf("branchy    %.3f ns/op%n", best / (double) N);

        best = Long.MAX_VALUE;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = callLoop(CALLS);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("callLoop checksum=" + sum);
        System.err.printf("callLoop   %.3f ns/op%n", best / (double) CALLS);
    }
}
