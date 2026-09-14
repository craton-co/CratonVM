// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/** Real javac control-flow fixtures for CUDA loop-body CFG lowering. */
public class EligibleBranchingLoop {
    // The two arms assign the same local before their join.  This pins the
    // lowering's explicit JVM-local merge registers, rather than merely
    // testing a branch whose arms both store directly to an array.
    public static void absOrIncrement(int[] in, int[] out) {
        int n = in.length;
        for (int i = 0; i < n; i++) {
            int value;
            if (in[i] < 0) {
                value = -in[i];
            } else {
                value = in[i] + 1;
            }
            out[i] = value;
        }
    }

    // A one-arm `if` with a fall-through loop back-edge verifies that the
    // false path can finish an iteration without a synthetic join block.
    public static void onlyNegatives(int[] in, int[] out) {
        int n = in.length;
        for (int i = 0; i < n; i++) {
            if (in[i] < 0) {
                out[i] = -in[i];
            }
        }
    }
}
