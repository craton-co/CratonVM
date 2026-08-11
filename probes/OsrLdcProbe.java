// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Settles the open question in
 * {@code osr-refused-for-a-loop-inline-in-main-20260810.md}: is {@code main}
 * special, or is the trigger the {@code ldc} that feeds the untaken call?
 *
 * <p>The refusal names {@code stack 1 (Unsupported) of 2} at the
 * {@code invokestatic}. {@code jit/src/x64/stack_kinds.rs} answers
 * {@link StackKind#Unknown} for a non-reference {@code ldc} — the constant is
 * int-or-float and the analysis is not told which — and the consult site turns
 * {@code Unknown} into {@code Unsupported}. In a method that also uses
 * {@code long}, that one entry makes the deopt point unresumable, and
 * {@code osr_exit_policy}'s artifact-wide veto then refuses OSR entry at every
 * pc of the method.
 *
 * <p>Four arms, one per run. Each has the same never-taken call on a
 * {@code long} operand stack, and the same hot loop that only OSR can rescue.
 * The only two things that vary are what feeds the call ({@code ldc} of a
 * {@code static final int} vs {@code getstatic} of a plain {@code static int})
 * and where the loop lives ({@code main} vs a once-called method):
 *
 * <pre>
 *   ldc-main     ldc,       loop in main            expect REFUSE
 *   var-main     getstatic, loop in main            expect OSR
 *   ldc-method   ldc,       loop in a called method expect REFUSE
 *   var-method   getstatic, loop in a called method expect OSR
 * </pre>
 *
 * If the two {@code var-} arms are fast and the two {@code ldc-} arms are slow,
 * the trigger is the {@code ldc} and {@code main} is not special. If instead
 * both {@code -main} arms are slow, {@code main} is.
 */
public final class OsrLdcProbe {

    /** Reached by {@code ldc} — a CONSTANT_Integer the kind analysis calls Unknown. */
    static final int NC = 40000000;

    /** Reached by {@code getstatic} — descriptor {@code I}, so the analysis says Int. */
    static int nv = 40000000;

    /** A {@code long} static, so the untaken call sits on a long operand stack. */
    static long sink;

    static long theLoop(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) {
            a += (i & 7) + 3;
        }
        return a;
    }

    /** The hot loop, plus a never-taken call fed by {@code ldc}. */
    static long inlineWithLdc(boolean never) {
        if (never) {
            sink += theLoop(NC);
        }
        long acc = 0;
        for (int i = 0; i < NC; i++) {
            acc += (i & 7) + 3;
        }
        return acc;
    }

    /** The hot loop, plus a never-taken call fed by {@code getstatic}. */
    static long inlineWithVar(boolean never) {
        if (never) {
            sink += theLoop(nv);
        }
        long acc = 0;
        for (int i = 0; i < NC; i++) {
            acc += (i & 7) + 3;
        }
        return acc;
    }

    public static void main(String[] args) {
        String arm = args.length > 0 ? args[0] : "ldc-main";
        // Never true, and not provably false to the compiler.
        boolean never = args.length > 99;
        long acc = 0;
        long t;

        if (arm.equals("ldc-main")) {
            if (never) {
                sink += theLoop(NC);
            }
            t = System.nanoTime();
            for (int i = 0; i < NC; i++) {
                acc += (i & 7) + 3;
            }
        } else if (arm.equals("var-main")) {
            if (never) {
                sink += theLoop(nv);
            }
            t = System.nanoTime();
            for (int i = 0; i < NC; i++) {
                acc += (i & 7) + 3;
            }
        } else if (arm.equals("ldc-method")) {
            t = System.nanoTime();
            acc = inlineWithLdc(never);
        } else {
            t = System.nanoTime();
            acc = inlineWithVar(never);
        }

        long el = System.nanoTime() - t;
        System.out.println("arm=" + arm + " ns/iter=" + (el / NC) + " ms=" + (el / 1000000)
                + " acc=" + acc + " sink=" + sink);
    }
}
