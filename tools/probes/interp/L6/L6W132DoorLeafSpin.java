// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 132, lane L6: a long-running form of
// `tools/probes/interp/L2/L2W25DoorSentinelBench.java` row `leaf`, for the
// Windows RIP sampler (stage 0 of
// docs/internal/proposals/interpreter/i25-L2-proposal-one-thread-block-for-the-interpreter-to-compiled-door-20260927.md,
// which had waited for a Linux `perf record`). An interpreted loop (run it with
// CRATONVM_JIT_OSR=0 so the loop is never OSR'd) calls a trap-free static
// leaf that compiles from its invocation counter, so every call goes through
// the interpreter-to-compiled door (`jit_bridge::run_jit_body_raw`).
//
// Argument: the number of reps of 20 000 000 calls (default 1). The sampler
// needs about 30 s of steady state: pass 40 or more.
//
// Run: javac -d out L6W132DoorLeafSpin.java
//      CRATONVM_JIT_OSR=0 cratonvm --jdk-only -cp out L6W132DoorLeafSpin [reps]
//
// HotSpot 25.0.3, default and -Xint, no argument:
// HOTSPOT_EXPECTED_BEGIN
// leaf reps=1 sum=200000010000000
// HOTSPOT_EXPECTED_END
public class L6W132DoorLeafSpin {
    static final int N = 20_000_000;

    static long leaf(long x) {
        return x + 1;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s = 0;
            for (int i = 0; i < N; i++) {
                s += leaf(i);
            }
            sum = s;
            System.out.println("TIME rep " + r + " ns/call=" + (System.nanoTime() - t0) / (double) N);
        }
        System.out.println("leaf reps=" + reps + " sum=" + sum);
    }
}
