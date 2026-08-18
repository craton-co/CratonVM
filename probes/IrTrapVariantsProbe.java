// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Isolates the precondition for
 * {@code jit/inline-trap-inside-a-protected-range-has-no-deopt-point-20260817}.
 *
 * The optimizing (IR) tier lowers an array access to a *deopt* on
 * out-of-bounds ({@code emit_array_null_bounds_guards} →
 * {@code DeoptReason::BoundsCheck}), intending the interpreter to re-execute
 * the opcode and throw with full semantics. That requires a precise resume.
 * The single-pass backend instead throws directly and routes the exception
 * through the method's exception table, which needs no resume at all.
 *
 * Each variant below is compiled separately (own method), so running with
 * {@code CRATONVM_JIT_FORCE_C2=1} says which of these three properties the
 * failure actually needs:
 *
 * <ol>
 *   <li>{@link #trapInTryWithSideEffects} — the doc's shape: trap inside a
 *       protected range, field stores before it.</li>
 *   <li>{@link #trapInTryNoSideEffects} — protected range, nothing mutated
 *       before the trap. If this survives, a whole-method replay was
 *       acceptable and the refusal is specifically about side effects.</li>
 *   <li>{@link #trapNoTryWithSideEffects} — side effects before the trap but
 *       NO exception table; the AIOOBE leaves the method. If this fails too,
 *       the protected range is not the precondition and the doc's title is
 *       describing a symptom rather than the cause.</li>
 * </ol>
 *
 * Prints one line per variant: {@code <name> OK} or {@code <name> INTERNALERROR}.
 * Exit code is non-zero if any variant raised {@code InternalError}.
 */
public final class IrTrapVariantsProbe {

    static final class Box {
        final byte[] states;
        int current;
        int next;
        int caught;

        Box(int n) {
            states = new byte[n];
            next = -1;
        }

        /** 1. Trap inside try, with stores before it. */
        void trapInTryWithSideEffects() {
            try {
                current = next;
                while (states[++next] != 1) {
                    // empty
                }
            } catch (ArrayIndexOutOfBoundsException e) {
                next = -2;
                caught++;
            }
        }

        /** 2. Trap inside try, nothing mutated before it. */
        int trapInTryNoSideEffects(int idx) {
            try {
                return states[idx];
            } catch (ArrayIndexOutOfBoundsException e) {
                return -1;
            }
        }

        /** 3. Stores before the trap, but no exception table in this method. */
        int trapNoTryWithSideEffects(int idx) {
            current = idx;
            next = idx + 1;
            return states[idx];
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int failures = 0;

        failures += run("trapInTryWithSideEffects", () -> {
            for (int i = 0; i < iterations; i++) {
                Box b = new Box(16);
                b.states[5] = 1;
                b.trapInTryWithSideEffects(); // stops at the marker
                b.next = 6;
                b.trapInTryWithSideEffects(); // runs off the end
                if (b.next != -2 || b.caught != 1) {
                    throw new AssertionError("state next=" + b.next + " caught=" + b.caught);
                }
            }
        });

        failures += run("trapInTryNoSideEffects", () -> {
            Box b = new Box(16);
            for (int i = 0; i < iterations; i++) {
                if (b.trapInTryNoSideEffects(3) != 0) {
                    throw new AssertionError("in-range read");
                }
                if (b.trapInTryNoSideEffects(99) != -1) {
                    throw new AssertionError("out-of-range read");
                }
            }
        });

        failures += run("trapNoTryWithSideEffects", () -> {
            Box b = new Box(16);
            for (int i = 0; i < iterations; i++) {
                if (b.trapNoTryWithSideEffects(3) != 0) {
                    throw new AssertionError("in-range read");
                }
                try {
                    b.trapNoTryWithSideEffects(99);
                    throw new AssertionError("expected AIOOBE");
                } catch (ArrayIndexOutOfBoundsException expected) {
                    // the exception must leave the method, as it does on HotSpot
                }
            }
        });

        if (failures != 0) {
            System.out.println("FAILURES=" + failures);
            System.exit(1);
        }
        System.out.println("ALL OK");
    }

    private static int run(String name, Runnable body) {
        try {
            body.run();
            System.out.println(name + " OK");
            return 0;
        } catch (InternalError e) {
            System.out.println(name + " INTERNALERROR " + e.getMessage());
            return 1;
        }
    }
}
