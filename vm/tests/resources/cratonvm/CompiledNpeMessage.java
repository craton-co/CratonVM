// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

/**
 * Fixture for {@code vm/tests/jit_npe_message_from_compiled_code.rs}.
 *
 * <p>Each entry point takes a warm-up switch so the SAME method can be driven
 * past the compile threshold on a path that returns normally, then tripped once
 * on its null path. The null deref therefore happens in an installed compiled
 * body, which is the only configuration under test — the interpreter's message
 * builder is a different code path and already worked.
 *
 * <p>{@code NULL_ARRAY} is a non-final static so the null must be reloaded with
 * {@code getstatic} at the deref, rather than being constant-folded into an
 * unconditional throw the null-check stub never runs for.
 */
public class CompiledNpeMessage {

    private static int[] NULL_ARRAY = null;

    /** {@code arraylength} on null — JEP 358 action {@code ARRAY_LENGTH}. */
    public static int lengthOfNull(int warm) {
        if (warm != 0) {
            int[] live = new int[4];
            return live.length;
        }
        return NULL_ARRAY.length;
    }

    /** {@code iaload} from a null {@code int[]} — action {@code ALOAD_INT}. */
    public static int loadFromNull(int warm) {
        if (warm != 0) {
            int[] live = new int[4];
            return live[0];
        }
        return NULL_ARRAY[0];
    }

    /** {@code iastore} into a null {@code int[]} — action {@code ASTORE_INT}. */
    public static int storeToNull(int warm) {
        if (warm != 0) {
            int[] live = new int[4];
            live[0] = warm;
            return live[0];
        }
        NULL_ARRAY[0] = warm;
        return 0;
    }
}
