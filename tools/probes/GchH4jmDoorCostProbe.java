// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gch h4/jm (2026-10-09, {@code gcd-d9d-call-argument-copies-still-rooted-on-other-call-shapes-20260928}):
 * the per-call cost of the interpreter-to-compiled door with reference
 * arguments, the one cost the d9d P3 + P2 flip has never isolated.
 *
 * <p>Under {@code CRATONVM_JIT_INTERP_DOOR_ARGS_WEAK=1} (P3) the door holds an
 * owning body's reference arguments as weak JNI global handles across the
 * run: two locked table operations per call ({@code WeakDoorArgs::take} adds,
 * its {@code Drop} removes). {@code BinT} and {@code MtChurnProbe} (the h3c
 * block Q rows) run almost entirely compiled-to-compiled, so they dilute that
 * cost to noise. Here {@code caller} is kept INTERPRETED
 * ({@code CRATONVM_JIT_DENY=GchH4jmDoorCostProbe.caller}) and {@code callee}
 * is a small static body that owns its arguments (no handler, not
 * synchronized, no trap), so every one of the {@code n} calls per round goes
 * through the door with two reference arguments.
 *
 * <p>Arguments: {@code [rounds [n]]}, default {@code 9 1000000}.
 *
 * <p>STDOUT (deterministic, identical on HotSpot 25): {@code door-cost checksum=<rounds*n/2>}.
 * STDERR: one {@code [door-cost] round=<r> ns_x10_per_call=<v>} line per round
 * and {@code [door-cost] median_ns_x10_per_call=<v>} (tenths of a nanosecond).
 */
public final class GchH4jmDoorCostProbe {
    private GchH4jmDoorCostProbe() {}

    /** The compiled callee: two reference arguments, no handler, no monitor. */
    static int callee(Object a, Object b, int i) {
        return (a == b ? 1 : 0) + (i & 1);
    }

    /** The interpreted caller (denied to the JIT by the rows). */
    static long caller(Object a, Object b, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += callee(a, b, i);
        }
        return s;
    }

    public static void main(String[] args) {
        final int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 9;
        final int n = args.length > 1 ? Integer.parseInt(args[1]) : 1_000_000;
        final Object a = new Object();
        final Object b = new byte[16];
        // Warm-up: the callee compiles; not timed, not in the checksum.
        caller(a, b, 50_000);
        caller(a, b, 50_000);
        long total = 0;
        final long[] tenths = new long[rounds];
        for (int r = 0; r < rounds; r++) {
            final long t0 = System.nanoTime();
            total += caller(a, b, n);
            final long dt = System.nanoTime() - t0;
            tenths[r] = n > 0 ? dt * 10 / n : 0;
            System.err.println("[door-cost] round=" + r + " ns_x10_per_call=" + tenths[r]);
        }
        java.util.Arrays.sort(tenths);
        if (rounds > 0) {
            System.err.println("[door-cost] median_ns_x10_per_call=" + tenths[rounds / 2]);
        }
        System.out.println("door-cost checksum=" + total);
    }
}
