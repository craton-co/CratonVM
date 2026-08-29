// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * `cond ? a : b`, the shape the branch-to-`selp` if-conversion exists for.
 *
 * javac compiles a ternary to a forward conditional branch over the
 * then-expression, a `goto` over the else-expression, and a join --
 * genuine control flow, even though the source says "pick one of two
 * values". Lowered literally that becomes a `bra` and, in the SASS
 * `ptxas` produces, a `BSSY`/`BSYNC` reconvergence pair around three
 * instructions of arithmetic. The kernels in `bench-gpu` are written
 * branchlessly on purpose and were getting branches anyway.
 *
 * `select` is the plain case. `nested` is the boundary: its else-arm
 * contains another ternary, so the OUTER diamond's arm is not a single
 * basic block and stays a branch while the inner one converts.
 * `withStore` must NOT convert at all -- an array store cannot be run
 * speculatively on both sides.
 */
public class EligibleTernary {

    /** One ternary over two float parameters. Converts. */
    public static void select(float[] a, float[] b, float[] out) {
        for (int i = 0; i < out.length; i++) {
            float x = a[i];
            float y = b[i];
            out[i] = x > y ? x - y : y - x;
        }
    }

    /** A ternary whose else-arm is another ternary: the inner one
     *  converts, the outer one keeps its branch. */
    public static void nested(float[] a, float[] out) {
        for (int i = 0; i < out.length; i++) {
            float s = a[i];
            out[i] = s > 1.0f ? 1.0f : (s < 0.0f ? 0.0f : s);
        }
    }

    /** Both arms store. Speculating either would write a cell the Java
     *  program does not write, so this must stay a branch. */
    public static void withStore(float[] a, float[] out) {
        for (int i = 0; i < out.length; i++) {
            if (a[i] > 0.0f) {
                out[i] = 1;
            } else {
                out[i] = 2;
            }
        }
    }
}
