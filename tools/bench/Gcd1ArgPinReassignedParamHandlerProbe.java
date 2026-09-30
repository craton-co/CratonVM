// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcd d9/d (2026-09-28): does a compiled callee's own handler, run by the
 * call site's callee-deopt service, see a PARAMETER the callee reassigned
 * before it threw?
 *
 * <p>The page
 * {@code docs/internal/gc/gcd-d9d-callee-handler-resume-hands-a-reassigned-parameter-its-entry-value-RETIRED-20260929.md}
 * explains the suspicion: the service's handler arm
 * ({@code vm/src/jit/helpers.rs}, {@code try_run_callee_handler} ->
 * {@code run_jit_callee_handler}) rebuilds the callee's frame from the call's
 * ENTRY arguments when no precise frame is stashed, and the compile gate that
 * decides whether a precise frame is needed
 * ({@code jit/src/lib.rs}, {@code local_handler_reads_unsafe_local}) counts
 * every parameter as safe at the handler, reassigned or not.
 *
 * <p>{@code f(x, s)} reassigns {@code x} and then parses {@code s}; its
 * handler returns {@code x}. A warmed caller calls it 120,000 times, one in
 * seven with an unparsable string, and sums the results.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -cp tools/bench Gcd1ArgPinReassignedParamHandlerProbe},
 * and {@code -Xint}) prints
 * <pre>
 *   sum=12816701880
 *   PASS reassigned-param-handler
 * </pre>
 * and exits 0; a handler that saw the entry value prints another sum and
 * {@code FAIL reassigned-param-handler}, exit 1.
 *
 * <p>Commands (the second arm takes the compiled local handlers out, which is
 * the configuration the suspected path needs):
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1ArgPinReassignedParamHandlerProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -cp tools/bench"
 *   timeout 300 cratonvm $P Gcd1ArgPinReassignedParamHandlerProbe
 *   CRATONVM_JIT_LOCAL_HANDLERS=0 timeout 300 cratonvm $P Gcd1ArgPinReassignedParamHandlerProbe
 * </pre>
 */
public final class Gcd1ArgPinReassignedParamHandlerProbe {
    static final String[] INPUTS = {"1", "22", "333", "4444", "55555", "666666", "z"};

    /** Reassigns its parameter, then throws; the handler reads the parameter. */
    static int f(int x, String s) {
        try {
            x = x * 2 + 1;
            return Integer.parseInt(s);
        } catch (NumberFormatException e) {
            return x;
        }
    }

    static long caller(int n) {
        long sum = 0;
        for (int i = 0; i < n; i++) {
            sum += f(10_000 + (i % 7), INPUTS[i % 7]);
        }
        return sum;
    }

    public static void main(String[] args) {
        long sum = 0;
        for (int round = 0; round < 120; round++) {
            sum += caller(1_000);
        }
        // Compared against the same sum computed without the exception path;
        // a handler that saw the entry `x` loses `x + 1` per unparsable call.
        System.out.println("sum=" + sum);
        boolean ok = sum == expected();
        System.out.println((ok ? "PASS" : "FAIL") + " reassigned-param-handler");
        if (!ok) {
            System.exit(1);
        }
    }

    /** The same sum, computed without the exception path. */
    static long expected() {
        long sum = 0;
        for (int round = 0; round < 120; round++) {
            for (int i = 0; i < 1_000; i++) {
                final int x = 10_000 + (i % 7);
                final String s = INPUTS[i % 7];
                sum += s.equals("z") ? x * 2 + 1 : Integer.parseInt(s);
            }
        }
        return sum;
    }
}
