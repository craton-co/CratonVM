// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 65, lane L2 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i64-L2-proposal-one-liveness-core-for-plan-slots-and-the-live-model-KEPT-OFF-20261028.md
// and
// docs/internal/jit-proposals/i64-L2-ranges-agree-is-false-for-every-graph-with-a-phi-FIXED-20261011.md.
//
// Two switches (the second default ON since 2026-10-11, `=0` to compare):
//   CRATONVM_JIT_IR_SHARED_LIVENESS_CORE=1  `ir_lower::plan_slots` and
//       `regalloc::build_live_model` run one liveness computation
//       (`regalloc::liveness_core`). A pure refactor: frames, allocations and
//       timings must be IDENTICAL with it on and off.
//   CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES=1  the slot plan also applies the
//       phi edge-interference rule, so its phi ranges equal the allocator's
//       and `ranges_agree` can hold in a loop. Frames must not move (phis are
//       pinned); only the split path's gate opens.
//
// Checks (stderr), on the methods below (`CRATONVM_DBG_IR_COMPILES=1` names
// them):
//   1. Refactor identity: run with `CRATONVM_DBG_IR_SLOTS=1
//      CRATONVM_DBG_IR_LINEAR_SCAN=1`, once with the core switch unset and
//      once `=1`; the `[ir-slots]` and `[ir] linear scan:` lines must diff
//      byte-identical.
//   2. The phi fix: `CRATONVM_JIT_IR_LS_SPLITS=1 CRATONVM_DBG_IR_LINEAR_SCAN=1`
//      prints `[ir-ls] splits: flag=1 ranges_agree=false` for every loop
//      method; adding `CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES=1` must turn it
//      `ranges_agree=true` (and `split_taken=` may then be non-zero).
//
// Rows (stderr, min ns per inner iteration over REPS=9 calls; JIT on):
//   sumLoop    `for (i) s += i`: one counter phi, one accumulator phi;
//   nested     two nested counted loops, `s += i + j`;
//   fpAcc      `sum += (i % 17) * 0.5`: a double accumulator phi live across
//              an integer remainder (the split candidate shape);
//   callLoop   `s += next(i)`: a phi live across a call in the loop body.
// A/B: the switch unset vs `=1`, one binary, interleaved runs, medians of the
// per-run minimums (in-JVM timings swing ~3x on a noisy host).
//
// stdout carries only checksums, identical on every VM and every switch:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// sumLoop checksum=499999500000
// nested checksum=2998000000
// fpAcc checksum=3999982.0
// callLoop checksum=14999950000
// HOTSPOT_EXPECTED_END
public class L2W65SharedLivenessBench {
    static final int REPS = 9;
    static final int N = 1000000;
    static final int OUTER = 2000;
    static final int INNER = 1000;
    static final int CALLS = 100000;

    static long sumLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long nested(int n, int m) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            for (int j = 0; j < m; j++) {
                s += i + j;
            }
        }
        return s;
    }

    static double fpAcc(int n) {
        double sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (i % 17) * 0.5;
        }
        return sum;
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
            sum = nested(OUTER, INNER);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("nested checksum=" + sum);
        System.err.printf("nested     %.3f ns/op%n", best / (double) ((long) OUTER * INNER));

        best = Long.MAX_VALUE;
        double d = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            d = fpAcc(N);
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println("fpAcc checksum=" + d);
        System.err.printf("fpAcc      %.3f ns/op%n", best / (double) N);

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
