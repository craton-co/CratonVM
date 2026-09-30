// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

/**
 * Fixture for {@code vm/tests/jit_bridge_sink_resumes_instead_of_rerunning.rs}.
 *
 * <p>Counts how many times a compiled body's side effect actually runs when
 * that body traps. A deopt sentinel does not mean "nothing happened": the
 * compiled body ran up to the trapping bci and stopped. A sink that answers it
 * by re-entering the method from bci 0 therefore runs everything before that
 * bci a SECOND time — silently.
 *
 * <p>The shape is the smallest one that makes the difference observable:
 *
 * <ul>
 *   <li>{@code SIDE_EFFECTS[0]++} is an {@code iastore}, which is
 *       {@code opcode_commits_side_effect} — and, unlike a {@code putstatic},
 *       one the optimizing IR front end can actually lower (it has no lowering
 *       for {@code 0xb3} and refuses the whole body on it);
 *   <li>{@code i / d} is an {@code idiv}, which the IR tier lowers to a
 *       <em>deopt guard</em> ({@code emit_deopt_if_zero},
 *       {@code DeoptReason::DivByZero}) rather than to a throw — so passing
 *       {@code d == 0} traps a compiled body instead of raising from it;
 *   <li>no {@code new} anywhere: an allocation-bearing method does not reach
 *       the optimizing tier at all without {@code CRATONVM_JIT_C2_ALLOC_UPGRADE},
 *       and a body the front end refuses measures nothing.
 * </ul>
 *
 * <p>{@code trip()} reads the counter either side of the trapping call and
 * returns the delta, so the test needs no heap poking: <b>1</b> is a precise
 * resume, <b>2</b> is a whole-method re-run that executed the store twice.
 */
public class DeoptRerunCount {

    /** The observable. An array, not a static int — see the class doc. */
    public static final int[] SIDE_EFFECTS = new int[1];

    /**
     * The body under test: a side effect, then a trap.
     *
     * <p>Not {@code private}: the caller must reach it through a real
     * {@code invokestatic} dispatch, which is what routes the deopt into
     * {@code execute_jit_call}'s sink rather than into {@code execute}'s.
     */
    static int hot(int i, int d) {
        SIDE_EFFECTS[0] = SIDE_EFFECTS[0] + 1;
        return i / d;
    }

    /** Drive {@code hot} on its non-trapping path, from bytecode. */
    public static int warm(int iters) {
        int acc = 0;
        for (int i = 0; i < iters; i++) {
            acc += hot(i, 7);
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
            hot(1, 0);
        } catch (ArithmeticException expected) {
            // The whole point of the trap; the count below is the measurement.
        }
        return SIDE_EFFECTS[0] - before;
    }

    /** Reset between arms, so a test can measure more than one call. */
    public static void reset() {
        SIDE_EFFECTS[0] = 0;
    }
}
