// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: ALLOC21-4's fresh array length fold
// (`CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD`,
// `ir_optimize::fold_fresh_array_lengths`: `ArrayLength(NewArray(.., Const(n)))`
// -> `Const(n)` for 0 <= n <= i32::MAX) with EA's unused-array elimination
// (`CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM`, primitive arrays up to 64 elements).
//
// Risk targeted: once the length read folds, the `newarray` may have no use
// left. The allocation must still THROW when it would: a negative constant
// (NegativeArraySizeException "-1"), a length past the VM limit
// ("Requested array size exceeds VM limit"), a length the heap cannot hold
// ("Java heap space" at -Xmx256m). A DCE or EA arm that deletes an unused
// `newarray` with a length it believes non-negative deletes the throw. Also:
// zero length, 64 / 65 (the elimination cap), arrays of references,
// multianewarray (`new int[3][4]`, not an `Op::NewArray`), a length from a
// static final / a spliced callee's constant argument, the length as a loop
// bound, `.length` inside a try (the exception-edge anchor refusal).
// Each shape sits behind a selector `k`; the warm-up runs the non-throwing
// selectors 200k times, then each throwing selector runs once (an unreached
// arm: the deopt trigger), then everything again.
//
// Run:
//   javac -d out R21Fz2ArrLen1.java
//   java -Xmx256m -cp out R21Fz2ArrLen1                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2ArrLen1
//   CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1 cratonvm -Xmx256m -cp out R21Fz2ArrLen1
//   CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1 CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1 cratonvm -Xmx256m -cp out R21Fz2ArrLen1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm. The two OOM texts
// are HotSpot's; a diff there on EVERY arm (default included) is a
// pre-existing message gap, not this feature.

public class R21Fz2ArrLen1 {
    static final int WARM = 200_000;
    static final int SF = 48;
    static int sink;

    static int sized(int n) {
        return new short[n].length;
    }

    static int len(int k) {
        switch (k) {
            case 0:
                return new int[0].length;
            case 1:
                return new byte[16].length;
            case 2:
                return new long[64].length;
            case 3:
                return new double[65].length;
            case 4:
                return new String[7].length;
            case 5:
                return new char[SF].length;
            case 6:
                return sized(33);
            case 7: {
                int[][] m = new int[3][4];
                return m.length * 100 + m[1].length * 10 + ((new int[2][])[0] == null ? 1 : 0);
            }
            case 8: {
                int[] a = new int[5];
                int s = 0;
                for (int i = 0; i < a.length; i++) {
                    a[i] = i * i;
                    s += a[i];
                }
                return s + a.length * 1000;
            }
            case 9: {
                try {
                    return new boolean[8].length;
                } catch (Throwable t) {
                    return -1;
                }
            }
            case 10: {
                int n = 12;
                int m = n * 2;
                return new float[m].length + new Object[n].length;
            }
            case 11: {
                byte[] b = new byte[1 << 20];
                return b.length >> 10;
            }
            // The throwing arms (never run in the warm-up).
            case 20:
                return new int[-1].length;
            case 21:
                return new long[Integer.MAX_VALUE].length;
            case 22:
                return new int[0x7fff_fff0].length;
            case 23:
                return sized(-5);
            case 24: {
                int n = -3;
                return new Object[n].length;
            }
            case 25:
                return new int[2][-2].length;
            default:
                return -99;
        }
    }

    static String one(int k) {
        try {
            return Integer.toString(len(k));
        } catch (NegativeArraySizeException e) {
            return "NASE(" + e.getMessage() + ")";
        } catch (OutOfMemoryError e) {
            return "OOM(" + e.getMessage() + ")";
        }
    }

    static void rows(String tag, int[] ks) {
        StringBuilder sb = new StringBuilder("row " + tag);
        for (int k : ks) {
            sb.append(" | ").append(k).append(':').append(one(k));
        }
        System.out.println(sb);
    }

    public static void main(String[] args) {
        int[] safe = {0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11};
        int[] throwing = {20, 21, 22, 23, 24, 25};
        rows("cold", safe);
        rows("cold-throw", throwing);
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            int k = r % 11; // 11 (the 1 MiB array) only once per 256 trips
            h = h * 31 + len(k);
            if ((r & 255) == 0) {
                h += len(11);
            }
        }
        System.out.println("warm hash " + h);
        rows("hot", safe);
        for (int k : throwing) {
            System.out.println("throw " + k + " " + one(k));
        }
        rows("after", safe);
        rows("after-throw", throwing);
        // The throwing arms hot: NASE only (cheap), 50k each.
        int nase = 0;
        for (int r = 0; r < 50_000; r++) {
            try {
                sink += len(20 + ((r & 1) * 3));
            } catch (NegativeArraySizeException e) {
                nase++;
            }
        }
        System.out.println("nase count " + nase);
        rows("final", safe);
        rows("final-throw", throwing);
    }
}
