// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package cratonvm;

/**
 * Regression probe for the OSR duplicate-frame fix
 * (docs/known-issues/jit/jit-self-recursive-activations-invisible-to-stack-walks-20260818.md).
 *
 * An OSR transfer hands control to compiled code part-way through a method that
 * is already running, and the interpreter's `Frame` for it stays on the thread.
 * Both halves then describe ONE activation, and the trace reported it twice:
 * `SWCross` read 69 frames where HotSpot reads 68, entries [0] and [1] both
 * `main`.
 *
 * `main` appears exactly once on any correct stack, whatever the tier, so this
 * needs no HotSpot comparison and a VM that never OSRs passes for the honest
 * reason rather than vacuously.
 *
 * The SHAPE matters and a simpler one does not work: a `main` whose loop body
 * is a plain call gets compiled whole rather than OSR-entered, and reports
 * `maxMainFrames=1` even with the fix disabled — a vacuous green. Capturing the
 * trace from the bottom of a DEEP recursion is what puts an OSR'd `main` under
 * a long compiled chain. Verified to bite: with
 * `CRATONVM_JIT_NO_OSR_FRAME_DEDUPE=1` this reports 2.
 */
public final class JitOsrFrameDedupeProbe {
    private static final int ROUNDS = 40;
    private static final int DEPTH = 64;

    public static void main(String[] args) {
        int worst = 0;
        for (int i = 0; i < ROUNDS; i++) {
            StackTraceElement[] t = grab(DEPTH);
            int n = 0;
            for (StackTraceElement e : t) {
                if (e.getClassName().equals("cratonvm.JitOsrFrameDedupeProbe")
                        && e.getMethodName().equals("main")) {
                    n++;
                }
            }
            if (n > worst) {
                worst = n;
            }
        }
        System.out.println("maxMainFrames=" + worst);
        System.out.println(worst == 1 ? "OSR_DEDUPE_OK" : "OSR_DEDUPE_DUPLICATED");
    }

    private static StackTraceElement[] grab(int d) {
        try {
            recurse(d);
            return new StackTraceElement[0];
        } catch (IllegalStateException e) {
            return e.getStackTrace();
        }
    }

    private static void recurse(int d) {
        if (d == 0) {
            throw new IllegalStateException("x");
        }
        recurse(d - 1);
    }
}
