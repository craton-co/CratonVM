// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Execution evidence for the bytecode loop rewriter (LOOP-01: peeling,
// unrolling, guarded versioning). Until this probe existed, the transforms had
// unit tests and nothing else — `docs/jit/loop-rewriter-wiring.md` listed
// "validated by anything larger than a unit test" as **no**.
//
// Run it twice and diff the output. The two runs must agree exactly:
//
//   cratonvm --java-home <jdk> -cp probes LoopXformProbe
//   CRATONVM_JIT='bytecode-loop-xform,deopt-real=0' \
//   CRATONVM_DBG='jit-gen' \
//     cratonvm --java-home <jdk> -cp probes LoopXformProbe
//
// BOTH tokens are needed. `bytecode-loop-xform` arms the rewriter (and turns
// the native byte-copy unroller off in the same motion — the two are exact
// complements). `deopt-real=0` clears the first of the four whole-compile
// refusals in `plan_bytecode_loop_xform`, which is default-ON and would
// otherwise refuse every compile before it looked at a loop.
//
// `CRATONVM_DBG=jit-gen` prints one line per rewrite:
//
//   [JIT_GEN] bytecode loop rewrite: kind=Unroll versioned=true header=… …
//
// Every method here is deliberately call-free and `invokedynamic`-free in its
// loop, because inline sites and indy are two of the other three whole-compile
// refusals. A loop that quietly acquired either would make this probe report
// "no rewrite" for a reason that has nothing to do with the loop.
public class LoopXformProbe {

    // The shape guarded versioning exists for: a runtime limit, so the
    // compile-time trip count is [0, Integer.MAX_VALUE] and `trip.min` is zero.
    // Called below with trip counts on BOTH sides of the guard's minimum, so a
    // guard that selected the wrong version would change the answer.
    static int accum(int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s = s + i * 3 + (i & 7);
        }
        return s;
    }

    // The same shape carrying a bounds check per iteration.
    static int sumArray(int[] a, int n) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += a[i];
        }
        return s;
    }

    // A nest. `detect_loops` order is innermost-first, so the inner loop is the
    // candidate and the outer one is only shifted.
    static int nest(int rows, int cols) {
        int s = 0;
        for (int r = 0; r < rows; r++) {
            for (int c = 0; c < cols; c++) {
                s += r ^ c;
            }
        }
        return s;
    }

    // A compile-time trip count: `prove_trip_count_at_least` answers `Static`,
    // so this one must be transformed WITHOUT a guard. A runtime compare here
    // would be testing a fact already known.
    static int fixedTrips() {
        int s = 0;
        for (int i = 0; i < 16; i++) {
            s += i * i;
        }
        return s;
    }

    // A loop that leaves early, so the exit branches inside the duplicated
    // copies are exercised rather than only the back edge.
    static int firstOver(int[] a, int n, int limit) {
        for (int i = 0; i < n; i++) {
            if (a[i] > limit) {
                return i;
            }
        }
        return -1;
    }

    public static void main(String[] args) {
        int[] data = new int[1024];
        for (int i = 0; i < data.length; i++) {
            data[i] = i * 31 + 7;
        }

        long acc = 0;
        // Warm-up and steady state. The trip counts straddle every guard
        // minimum the planner can ask for (2 and 4), so both versions of every
        // versioned loop run many times.
        for (int rep = 0; rep < 60000; rep++) {
            acc += accum(rep % 13);
            acc += sumArray(data, rep % 97);
            acc += nest(rep % 7, rep % 5);
            acc += fixedTrips();
            acc += firstOver(data, rep % 61, rep * 7);
        }

        // One long-running loop, so the compiler tiers up while the loop is on
        // the stack and an OSR entry has to land in a transformed method.
        acc += accum(3000000);
        acc += sumArray(data, data.length) * 3L;

        // Degenerate trip counts, after the methods are hot: 0 and 1 are where
        // a transform that assumed a minimum would be caught.
        for (int rep = 0; rep < 5; rep++) {
            acc += accum(0);
            acc += accum(1);
            acc += sumArray(data, 0);
            acc += sumArray(data, 1);
            acc += nest(0, 0);
            acc += nest(1, 1);
            acc += firstOver(data, 0, 0);
        }

        System.out.println("acc=" + acc);
    }
}
