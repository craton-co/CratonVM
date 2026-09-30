// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

/**
 * Fixture for {@code vm/tests/jit_lambda_door_deopt_resumes.rs} — the LAMBDA
 * door's half of the trapping-body question.
 *
 * <p>{@code DeoptRerunCount} is the sibling fixture and the one to read first:
 * it explains why the side effect is an {@code iastore}, why the trap is an
 * {@code idiv}, and why {@code new} must not appear. This file repeats that
 * shape inside a SAM implementation instead of a {@code static} method, and it
 * exists because of a gap that fixture cannot close.
 *
 * <h2>Why a separate fixture</h2>
 *
 * <p>{@code jit_bridge_sink_resumes_instead_of_rerunning.rs} asserts
 * {@code lambda_site_deopt_outcomes().1 == 0} — the counter whose own doc calls
 * it "the metric a regression test asserts is zero". But
 * {@code DeoptRerunCount} contains <b>no SAM anywhere</b>: it is
 * {@code invokestatic} throughout. Neither {@code SITE_RESUMED} nor
 * {@code SITE_UNRESUMABLE} can be incremented by that fixture, so the zero it
 * asserts is <em>structural</em> — it would hold just as well on a VM whose
 * lambda door was completely broken. A zero from a counter nothing bumped is
 * evidence about the counter, not about the VM.
 *
 * <p>So this fixture drives the third sink ({@code resume_deopted_body}, shared
 * by {@code execute_jit_call_oneshot} and the compiled caller's
 * {@code try_lambda_site_direct_call}) for real, and its test asserts the
 * ENGAGEMENT — {@code resumed > 0} — beside the property, so the zero has to be
 * earned.
 *
 * <h2>The shape</h2>
 *
 * <p>{@code OP} is a {@code static final} lambda, so it is created once by
 * {@code invokedynamic} in {@code <clinit>} and every call afterwards is an
 * ordinary {@code invokeinterface} on the same instance — which is what lets
 * the site install a direct thunk and take the one-shot door. The lambda BODY
 * becomes a synthetic {@code static} method, and that is what the optimizing
 * tier compiles and what traps.
 *
 * <p>{@code warm} drives the non-trapping path from bytecode; {@code trip}
 * reads the counter either side of one trapping call and returns the delta.
 * <b>1</b> is a precise resume at the trapping bci, <b>2</b> is a whole-method
 * re-run that executed the store a second time.
 */
public class DeoptLambdaRerunCount {

    /** The SAM. Two ints in, one out — no boxing, no allocation. */
    public interface IntOp {
        int apply(int i, int d);
    }

    /** The observable. An array, not a static int — see {@code DeoptRerunCount}. */
    public static final int[] SIDE_EFFECTS = new int[1];

    /**
     * The body under test: a side effect, then a trap.
     *
     * <p>{@code static final} so the capture is constant and the call site is
     * monomorphic from its first execution.
     */
    public static final IntOp OP = (i, d) -> {
        SIDE_EFFECTS[0] = SIDE_EFFECTS[0] + 1;
        return i / d;
    };

    /**
     * The SAM call site, and it must be COMPILED when the trap fires.
     *
     * <p>This indirection is the whole reason the fixture is shaped this way.
     * {@code try_lambda_site_direct_call} — one of the two doors that reach
     * {@code resume_deopted_body} — is entered from COMPILED CODE
     * ({@code jit_invoke_virtual_mic} and the sibling door in
     * {@code jit/helpers.rs}), never from an interpreted frame. A first draft
     * put {@code OP.apply(1, 0)} directly in {@code trip()}, which runs
     * interpreted: the impl was compiled and did trap, but the deopt went to
     * the ordinary interface sink and both lambda-site counters stayed at zero.
     *
     * <p>So {@code step} is driven hot by {@code warm}, gets an artifact of its
     * own, and by the time {@code trip()} asks it for the trapping divisor its
     * {@code OP.apply} is a call site in machine code.
     */
    public static int step(int d) {
        return OP.apply(1, d);
    }

    /** Drive the lambda AND its call site on the non-trapping path. */
    public static int warm(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += step(7);
        }
        return acc;
    }

    /**
     * One trapping call. Returns how many times the side effect ran for it.
     *
     * <p>The {@code catch} is the point: after the deopt the interpreter
     * re-executes the {@code idiv} and throws, and that throw is ordinary
     * control flow the caller handles — so the count, not the exception, is
     * what this fixture reports.
     */
    public static int trip() {
        int before = SIDE_EFFECTS[0];
        try {
            step(0);
        } catch (ArithmeticException expected) {
            // The whole point of the trap; the count below is the measurement.
        }
        return SIDE_EFFECTS[0] - before;
    }

    // ---------------------------------------------------------------------
    // NON-LAMBDA CONTROL
    // ---------------------------------------------------------------------
    //
    // The same two-frame shape with plain `invokestatic` and no SAM anywhere:
    // a COMPILED caller ({@code stepStatic}) invoking a COMPILED callee
    // ({@code implStatic}) that traps.
    //
    // This is the discriminator. `DeoptRerunCount` already covers ONE frame —
    // an interpreted caller invoking a compiled callee that traps — and its
    // delta is 1. If this control also reports 2, the duplication has nothing
    // to do with lambdas and everything to do with the extra COMPILED frame
    // between the interpreted caller and the trap.

    /** Same body as the lambda, reached by `invokestatic`. */
    static int implStatic(int i, int d) {
        SIDE_EFFECTS[0] = SIDE_EFFECTS[0] + 1;
        return i / d;
    }

    /** The compiled caller — `step`'s twin, without the SAM. */
    public static int stepStatic(int d) {
        return implStatic(1, d);
    }

    /** Drive both control frames on the non-trapping path. */
    public static int warmStatic(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += stepStatic(7);
        }
        return acc;
    }

    /** One trapping call through the control shape. */
    public static int tripStatic() {
        int before = SIDE_EFFECTS[0];
        try {
            stepStatic(0);
        } catch (ArithmeticException expected) {
            // As above; the count is the measurement.
        }
        return SIDE_EFFECTS[0] - before;
    }
}
