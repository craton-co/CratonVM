// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * One loop, two placements, selected by argv so a single run contains exactly
 * one of them.
 *
 * <p>{@code main} is invoked once, so OSR is the only route out of the
 * interpreter for a loop written inline in it; a loop in {@link #theLoop} can
 * leave through an ordinary invocation-counter tier-up instead. If the two arms
 * report the same ns/iter, OSR entry into {@code main} works. If the inline arm
 * is orders of magnitude slower, the entry was refused and 40M iterations ran
 * interpreted.
 *
 * <p>Both arms are compiled into every run on purpose: the refusal recorded in
 * {@code osr-refused-for-a-loop-inline-in-main-20260810.md} came from a deopt
 * point inside the {@code method} arm — code the {@code main} arm never
 * executes — so deleting the unused arm would delete the trigger.
 *
 * <p>Run each arm in its own process:
 * {@code OsrProbe method} and {@code OsrProbe main}.
 */
public final class OsrProbe {

    static final int N = 40000000;

    /** Static, and a {@code long}: the getstatic that precedes the call in the
     * {@code method} arm is what puts a long on the operand stack at the deopt
     * point the refusal named. */
    static long sink;

    static long theLoop(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) {
            a += (i & 7) + 3;
        }
        return a;
    }

    public static void main(String[] args) {
        String arm = args.length > 0 ? args[0] : "main";
        if (arm.equals("method")) {
            sink += theLoop(N);
            long t = System.nanoTime();
            sink += theLoop(N);
            long el = System.nanoTime() - t;
            System.out.println("arm=method ns/iter=" + (el / N) + " ms=" + (el / 1000000)
                    + " sink=" + sink);
        } else {
            long acc = 0;
            long t = System.nanoTime();
            for (int i = 0; i < N; i++) {
                acc += (i & 7) + 3;
            }
            long el = System.nanoTime() - t;
            System.out.println("arm=main ns/iter=" + (el / N) + " ms=" + (el / 1000000)
                    + " acc=" + acc);
        }
    }
}
