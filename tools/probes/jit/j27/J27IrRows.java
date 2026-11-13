// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 4, lane IRROWS: facts for the "JIT IR / lowering" and
 * "JIT codegen / regalloc" rows of
 * docs/feature-designs/jit-ir-optimizations.md.
 *
 * IRLOWER17-3 (census and opt-in refusal of undominated by-bci anchors): each
 * row warms a loop whose by-bci frames name loop values (a hoisted invariant,
 * a value merged from two arms), then runs the input that throws inside the
 * loop, so the exception transfer at that bci must see every local exactly.
 * The answers must not change in any arm:
 *   default
 *   CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR=1
 *   CRATONVM_JIT_IR_BCI_ANCHOR_DOMINATED=0
 * and the `[ir] bci anchors undominated=` lines under CRATONVM_DBG_IR_COMPILES=1
 * are the census (absence means every anchor was honest).
 *
 * Expected HotSpot 25 output (all lines, in order):
 *
 *   fact licm-warm-mismatches 0
 *   fact licm-throw 4100
 *   fact licm-rewarm-mismatches 0
 *   fact merge-warm-mismatches 0
 *   fact merge-div-trap 120010
 *   fact merge-rewarm-mismatches 0
 *
 * No timing lines.
 */
public final class J27IrRows {
    static final int WARM = 20_000;

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    /** `k * m + 3` is loop-invariant; `a[i]` throws past the array's end. */
    static int licmThrow(int[] a, int k, int m) {
        int acc = 0;
        int i = 0;
        try {
            for (i = 0; i < 1000; i++) {
                int inv = k * m + 3;
                acc += a[i] + inv;
            }
        } catch (ArrayIndexOutOfBoundsException e) {
            return acc * 31 + i;
        }
        return -acc;
    }

    /** `x` merges two arms; `x / d[i]` throws on a zero divisor. */
    static long mergeDiv(int[] d, int n) {
        long s = 0;
        int i = 0;
        try {
            for (i = 0; i < n; i++) {
                int x = (i & 1) == 0 ? i * 3 : i + 7;
                s += x / d[i];
            }
        } catch (ArithmeticException e) {
            return s * 1000 + i;
        }
        return s;
    }

    static int[] iota(int n) {
        int[] a = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
        }
        return a;
    }

    static int[] ones(int n) {
        int[] a = new int[n];
        java.util.Arrays.fill(a, 1);
        return a;
    }

    static int licmMismatches(int[] full) {
        int bad = 0;
        for (int r = 0; r < WARM; r++) {
            if (licmThrow(full, 2, 5) != -512500) {
                bad++;
            }
        }
        return bad;
    }

    static int mergeMismatches(int[] d) {
        int bad = 0;
        for (int r = 0; r < WARM; r++) {
            if (mergeDiv(d, 64) != 4224L) {
                bad++;
            }
        }
        return bad;
    }

    public static void main(String[] args) {
        int[] full = iota(1000);
        fact("licm-warm-mismatches", licmMismatches(full));
        fact("licm-throw", licmThrow(iota(8), 2, 5));
        fact("licm-rewarm-mismatches", licmMismatches(full));

        int[] d = ones(64);
        fact("merge-warm-mismatches", mergeMismatches(d));
        int[] zeroAt10 = ones(64);
        zeroAt10[10] = 0;
        fact("merge-div-trap", mergeDiv(zeroAt10, 64));
        fact("merge-rewarm-mismatches", mergeMismatches(d));
    }
}
