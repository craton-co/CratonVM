// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Reproducer for
 * {@code jit/inline-trap-inside-a-protected-range-has-no-deopt-point-20260817}.
 *
 * Reduces {@code OpenIntToDoubleHashMap$Iterator.advance()} — the Apache
 * Commons Math method that crashed {@code SparseRealVectorTest} with a hard
 * {@code InternalError} — to its load-bearing shape, with no dependencies:
 *
 * <ul>
 *   <li>an array-bounds exception used as loop control, so the trapping
 *       bytecode is an inline {@code baload}, not a call;</li>
 *   <li>that {@code baload} sits inside an exception-table range, so the
 *       invoke-site {@code PendingException} machinery does not cover it;</li>
 *   <li><b>field stores happen on every iteration before the trap</b>, which is
 *       what makes a whole-method replay observably wrong and forces the
 *       "refusing side-effecting replay" refusal rather than a silent re-run.</li>
 * </ul>
 *
 * The last point is the one to preserve if this is ever simplified: without a
 * side effect ahead of the trap, a replay-from-entry fallback would be
 * harmless and the bug would not reproduce.
 *
 * <p>Run it hot enough to be compiled, then keep going. Correct output is
 * {@code OK} and exit 0; the bug is an {@code InternalError} escaping from
 * {@link Iter#advance()}.
 */
public final class InlineTrapInTryProbe {

    /** Marker byte meaning "this slot is occupied", as in the original. */
    private static final byte FULL = 1;

    /** The reduced {@code OpenIntToDoubleHashMap$Iterator}. */
    static final class Iter {
        private final byte[] states;
        /** Side effect #1: written every iteration, before the trap. */
        int current = -1;
        /** Side effect #2: incremented every iteration, before the trap. */
        int next;
        /** Counts how many times the catch arm ran, so the caller can check it. */
        int exhaustedCount;

        Iter(byte[] states) {
            this.states = states;
            this.next = -1;
        }

        /**
         * The shape under test. `++next` and `current = next` both mutate this
         * object before the `baload` that eventually throws, and the throw is
         * caught by this method's own exception table.
         */
        void advance() {
            try {
                current = next;
                while (states[++next] != FULL) {
                    // deliberately empty: the array read IS the loop condition
                }
            } catch (ArrayIndexOutOfBoundsException e) {
                next = -2;
                exhaustedCount++;
            }
        }

        boolean exhausted() {
            return next == -2;
        }
    }

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;

        // A states array with a few FULL slots and a tail of empties, so most
        // advance() calls stop at a marker and the last one runs off the end.
        byte[] states = new byte[16];
        states[3] = FULL;
        states[7] = FULL;
        states[11] = FULL;

        long visited = 0;
        long exhausted = 0;
        for (int i = 0; i < iterations; i++) {
            Iter it = new Iter(states);
            // Walk to each FULL marker, then once more to run off the end.
            for (int k = 0; k < 4; k++) {
                it.advance();
                if (it.exhausted()) {
                    exhausted++;
                    break;
                }
                visited += it.next;
            }
        }

        // Each outer iteration visits 3 markers (at 3, 7, 11) and then exhausts.
        long wantVisited = 3L + 7L + 11L;
        if (visited != wantVisited * iterations) {
            throw new AssertionError("visited=" + visited + ", want " + (wantVisited * iterations));
        }
        if (exhausted != iterations) {
            throw new AssertionError("exhausted=" + exhausted + ", want " + iterations);
        }

        // The same shape where the FIRST read is already out of bounds, so the
        // trap fires on iteration 0 with `current` freshly stored.
        Iter empty = new Iter(new byte[0]);
        empty.advance();
        if (!empty.exhausted() || empty.exhaustedCount != 1) {
            throw new AssertionError("empty: next=" + empty.next + " count=" + empty.exhaustedCount);
        }

        System.out.println("OK");
    }
}
