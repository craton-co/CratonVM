// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// A workload of NOTHING BUT zero-argument static calls, for reading the
// `CRATONVM_DBG_INVOKE_PHASES` breakdown.
//
// InvokeFrameCostProbe cannot be used for that reading. Its kernels call
// methods with 0, 4 and 8 arguments, and the phase counters are process-wide,
// so its `args` phase aggregates all three arities — it answers "what does
// argument handling cost on average across this probe", not "what does a
// zero-argument call cost", which is the fixed cost under investigation.
//
// Here every instrumented call takes no arguments and returns a constant, so
// the `args` phase measures only the per-call part of argument handling
// (`ParamTags::of` and the loop preamble that runs even when the loop body does
// not). If it is still a large share, the per-CALL half of argument handling is
// bigger than the per-ARGUMENT half — which would mean the 2026-08-18
// argument-scan hoist addressed the smaller of the two.
public final class ZeroArgCallProbe {

    static int callee() { return 1; }

    static int kernel(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += callee(); acc += callee(); acc += callee(); acc += callee();
            acc += callee(); acc += callee(); acc += callee(); acc += callee();
            acc += callee(); acc += callee(); acc += callee(); acc += callee();
            acc += callee(); acc += callee(); acc += callee(); acc += callee();
        }
        return acc;
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        int g = 0;
        long t0 = System.nanoTime();
        g += kernel(n);
        long dt = System.nanoTime() - t0;
        System.out.printf("zero-arg calls: %d  wall %.1f ns/call%n",
                (long) n * 16, (double) dt / ((long) n * 16));
        System.out.println("guard " + g);
    }
}
