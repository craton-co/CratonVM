// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The loop bound written as a bare `int` parameter rather than an array
 * length.
 *
 * javac compiles `for (int i = 0; i < n; i++)` with `n` a parameter to
 *
 *     iload iv; iload n; if_icmpge exit
 *
 * which is byte-for-byte the shape a HOISTED `arr.length` produces --
 * the recognizer told them apart by scanning the pre-loop for an
 * `arraylength` that stored into that local, and rejected the bound when
 * it found none.
 *
 * Accepting it is not free the way the inline-`arraylength` form was.
 * The launch grid is otherwise sized from the largest array argument,
 * and a scalar bound may exceed every one of them, so a kernel admitted
 * without telling the host would run fewer threads than the Java loop
 * has iterations. A thread that is never created reaches no bounds
 * check, so the result would be silently partial rather than a deopt.
 * `WorkBound::ParamScalar` is the other half of this acceptance: it
 * carries the parameter index to the dispatch site, which sizes the grid
 * from `max(largest array length, that parameter's runtime value)`.
 */
public class EligibleScalarBound {

    /** The bound is parameter 2, never stored to. Accepted. */
    public static void scaleN(int[] in, int[] out, int n) {
        for (int i = 0; i < n; i++) {
            out[i] = in[i] * 3;
        }
    }

    /**
     * The bound parameter is REASSIGNED before the loop, so the local is
     * no longer provably the incoming argument at the header. Rejected:
     * the host would size the grid from the argument it was handed, not
     * from the value the guard actually compares against.
     */
    public static void scaleClamped(int[] in, int[] out, int n) {
        n = n - 1;
        for (int i = 0; i < n; i++) {
            out[i] = in[i] * 3;
        }
    }

    /**
     * A scalar bound on a per-row reduction: the outer loop is the
     * parallel dimension and `cols` is a real sequential inner loop the
     * thread runs itself. This is the shape a matrix-vector product has
     * once the row count stops being an array length.
     */
    public static void rowSums(float[] m, int rows, int cols, float[] out) {
        for (int i = 0; i < rows; i++) {
            float sum = 0.0f;
            for (int j = 0; j < cols; j++) {
                sum += m[i * cols + j];
            }
            out[i] = sum;
        }
    }

    /**
     * Both levels of a rectangular 2-D nest bounded by scalars. Rejected
     * -- the flattened trip count is `rows * cols`, a PRODUCT, and
     * `WorkBound` names one parameter, so the host would fall back to
     * the largest-array rule and silently skip every `(i, j)` past it.
     */
    public static void fillRect(int[] out, int rows, int cols) {
        for (int i = 0; i < rows; i++) {
            for (int j = 0; j < cols; j++) {
                out[i * cols + j] = i + j;
            }
        }
    }
}
