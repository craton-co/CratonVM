// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L2 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i70-L2-proposal-one-predecessor-index-per-schedule-KEPT-OFF-20261104.md
// (and the block-count gate the round-i2 handoff asks for).
//
// Switches (all opt-in; each a pure refactor of the optimizing tier's
// liveness, so compiled code, frames and steady-state timings must be
// IDENTICAL with any combination on or off):
//   CRATONVM_JIT_IR_LIVENESS_WORKLIST=1          (wave 69) the shared core's
//       fixed point through the dirty-block worklist.
//   CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST=1   (wave 70) the four legacy
//       copies the default JIT runs, through the same worklist.
//   CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS=<n>  (wave 71, knob, unset =
//       no gate) either worklist runs only on schedules of at least n blocks;
//       smaller ones keep the round robin.
//   CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS=1    (wave 71) the worklist reads
//       each block's own predecessor list instead of building a predecessor
//       index per fixed point.
//   CRATONVM_JIT_IR_LIVENESS_POSTORDER=1         (wave 71) the worklist's passes
//       visit the blocks in CFG postorder instead of reverse layout index
//       (same converged answer; only a run cut off by the cap can differ,
//       which none of the methods below comes near).
//
// The real measurement is the Rust bench (ns per run by block count, and a
// `crossover` line per family saying from which block count the worklist
// wins):
//   cargo test -p cratonvm-jit --release --lib i71_l2_bench -- --ignored --nocapture
//
// Checks on the methods below, which span the block counts the gate is about
// (`tiny` 1 block, `oneLoop` a handful, `medium` about 16, `large` 16
// sequential loops, 40+):
//   1. Byte identity: with CRATONVM_DBG_IR_SLOTS=1 CRATONVM_DBG_IR_LINEAR_SCAN=1,
//      run with no switch, then with
//        CRATONVM_JIT_IR_LIVENESS_WORKLIST=1 CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST=1
//      plus each of: nothing, CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS=1,
//      CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS=40 (and both), and
//      CRATONVM_JIT_IR_LIVENESS_POSTORDER=1; the
//      `[ir-slots]` and `[ir] linear scan:` lines must diff empty.
//   2. Compile time: `CRATONVM_DBG_IR_COMPILES=1` per-compile timings for the
//      four methods under the same env sets. Expected: the gate at 24-40
//      keeps `tiny` / `oneLoop` / `medium` at the round-robin time and
//      `large` at the worklist time; SCHEDULE_PREDS shaves the per-call
//      index build off every worklist run.
//
// Rows (stdout, `ns/op`, min over REPS calls; JIT on) are steady-state code,
// which no switch may change: the regression guard, not the expected win.
// A/B: one binary, interleaved runs, medians of the per-run minimums.
//
// Non-timing stdout lines are checksums, identical on every VM and switch:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// tiny checksum=1499999500000
// oneLoop checksum=4999950000
// medium checksum=22500300000
// large checksum=27198640000
// HOTSPOT_EXPECTED_END
public class L2W71LivenessWorklistGateBench {
    static final int REPS = 9;
    static final int TINY_N = 1000000;
    static final int ONE_N = 100000;
    static final int MEDIUM_N = 300000;
    static final int LARGE_N = 20000;

    static int tiny(int x) {
        return x * 3 + 1;
    }

    static long tinyDriver(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += tiny(i);
        }
        return s;
    }

    static long oneLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
        }
        return s;
    }

    static long medium(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            int r = i % 3;
            if (r == 0) {
                s += 1;
            } else if (r == 1) {
                s += 2;
            } else {
                s += 3;
            }
        }
        for (int j = 0; j < n; j++) {
            if ((j & 1) == 0) {
                s += j;
            } else {
                s -= 1;
            }
        }
        return s;
    }

    static long large(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) { s += i; }
        for (int i = 0; i < n; i++) { s += 2L * i; }
        for (int i = 0; i < n; i++) { s += 3L * i; }
        for (int i = 0; i < n; i++) { s += 4L * i; }
        for (int i = 0; i < n; i++) { s += 5L * i; }
        for (int i = 0; i < n; i++) { s += 6L * i; }
        for (int i = 0; i < n; i++) { s += 7L * i; }
        for (int i = 0; i < n; i++) { s += 8L * i; }
        for (int i = 0; i < n; i++) { s += 9L * i; }
        for (int i = 0; i < n; i++) { s += 10L * i; }
        for (int i = 0; i < n; i++) { s += 11L * i; }
        for (int i = 0; i < n; i++) { s += 12L * i; }
        for (int i = 0; i < n; i++) { s += 13L * i; }
        for (int i = 0; i < n; i++) { s += 14L * i; }
        for (int i = 0; i < n; i++) { s += 15L * i; }
        for (int i = 0; i < n; i++) { s += 16L * i; }
        return s;
    }

    interface Body {
        long run();
    }

    static void row(String name, Body body, long ops) {
        long best = Long.MAX_VALUE;
        long sum = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            sum = body.run();
            best = Math.min(best, System.nanoTime() - t0);
        }
        System.out.println(name + " checksum=" + sum);
        System.out.printf("%-8s %.3f ns/op%n", name, best / (double) ops);
    }

    public static void main(String[] args) {
        row("tiny", () -> tinyDriver(TINY_N), TINY_N);
        row("oneLoop", () -> oneLoop(ONE_N), ONE_N);
        row("medium", () -> medium(MEDIUM_N), 2L * MEDIUM_N);
        row("large", () -> large(LARGE_N), 16L * LARGE_N);
    }
}
