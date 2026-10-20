// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L2 -- the bench of
// CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY (default off; the narrow
// slice of i39-L2-proposal-a-compile-request-returns-why-it-produced-no-body).
// A long counted loop whose body names classes nothing has loaded yet (a
// static field, a static call, an instance field and an allocation of four
// "Late*" classes, each behind a branch the first activations never take).
// The first OSR offers compile the optimizing body while those classes are
// unloaded; when the compile declines at a constant-pool resolver miss, the
// route memoises the refusal method-wide (switch off) and every later
// activation keeps the single-pass OSR body, or retries (switch on, at most
// four times per method) and gets the optimizing body once the classes are
// loaded.
//
// Phases: "cold" (8 activations, the branches untaken, the Late classes not
// loaded), then one activation that takes every branch (loads them), then
// "warm" (best of `reps` timed batches of 8 activations, branches untaken).
// Timings on lines starting "TIME "; stdout otherwise only the checksum lines.
//
// A/B (one binary, interleaved, medians; both modes):
//   cratonvm L2W101OsrResolverMissBench 5
//   CRATONVM_JIT_OSR_OPTIMIZING_RESOLVER_MISS_RETRY=1 cratonvm L2W101OsrResolverMissBench 5
//   (java L2W101OsrResolverMissBench 5 for HotSpot)
// Census first: CRATONVM_DBG_JITC=1 must show "compile declined at a resolver
// miss (not memoised; retried later)" for `spin` with the switch on; if it
// shows "compile declined" with no resolver miss, or no decline at all, this
// workload does not reach the route's resolver-miss memo and the switch has
// nothing to win here (record that).
//
// HOTSPOT_EXPECTED_BEGIN
// checksum cold=true late=true warm=true
// HOTSPOT_EXPECTED_END

public class L2W101OsrResolverMissBench {
    static final class LateStatic {
        static int VALUE = 3;
    }

    static final class LateCall {
        static int f(int i) {
            return i & 7;
        }
    }

    static final class LateField {
        int v = 5;
    }

    static final class LateNew {
        final int w;

        LateNew(int w) {
            this.w = w;
        }
    }

    static Object holder;

    static long spin(int n, boolean late, LateField lf) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s = s * 31 + (i ^ (s >>> 9));
            if (late && i == n - 1) {
                s += LateStatic.VALUE;
                s += LateCall.f(i);
                s += lf.v;
                LateNew x = new LateNew(i);
                holder = x;
                s += x.w;
            }
        }
        return s;
    }

    static final int N = 3_000_000;

    static long batch(boolean late) {
        long sum = 0;
        for (int k = 0; k < 8; k++) {
            sum += spin(N + k, late, null);
        }
        return sum;
    }

    // Plain Java model of `spin` with the branches untaken, for the checksum.
    static long model(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s = s * 31 + (i ^ (s >>> 9));
        }
        return s;
    }

    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        long t0 = System.nanoTime();
        long cold = batch(false);
        long coldMs = (System.nanoTime() - t0) / 1_000_000;
        long expect = 0;
        for (int k = 0; k < 8; k++) {
            expect += model(N + k);
        }
        // Loads the four Late classes.
        long late = spin(1000, true, new LateField());
        long lateExpect = model(1000);
        lateExpect += 3 + (999 & 7) + 5 + 999;
        long best = Long.MAX_VALUE;
        boolean warmOk = true;
        for (int r = 0; r < reps; r++) {
            long t1 = System.nanoTime();
            long warm = batch(false);
            best = Math.min(best, (System.nanoTime() - t1) / 1_000_000);
            warmOk &= warm == expect;
        }
        System.out.println("TIME cold ms=" + coldMs);
        System.out.println("TIME warm ms=" + best);
        System.out.println("checksum cold=" + (cold == expect) + " late=" + (late == lateExpect)
                + " warm=" + warmOk);
    }
}
