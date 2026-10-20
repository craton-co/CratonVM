// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L7: the fused array element
// increment's deferred operands,
//   CRATONVM_INTERP_FUSED_ARRAY_OPERAND_MISS_BUDGET            a pc whose
//       operand keeps missing (a `<clinit>` static, a short operand array)
//       is retired into the decline memo after 64 misses (default off)
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND_WIDE   the element
//       operand from `aload n` / `iload n` and with an `iload; <const>;
//       iadd | isub` index (default off; needs
//       CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND, default on).
// No behaviour change: every line is the same with each switch off and on,
// with and without --nojit, and with CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO
// off and on.
//
// Checked: a `<clinit>` loop over its own static (never a filled site), a
// short operand array that throws on every call for more than the budget
// (and the element left unchanged each time), then the same site hitting;
// `p[j] += p[j - 1]` (prefix sum), its AIOOBE at j = 0 with the element
// unchanged, `h[j] += w[j + 1]` (bipush and sipush offsets), the operand
// array in local 4 and the operand index in local 4, a long and a narrowed
// byte group with an index expression, and the value form.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// clinit 250 250 250 250
// misses 100 Index 5 out of bounds for length 2 a=0
// after misses 40
// prefix 1 3 6 10
// prefix oob 100 Index -1 out of bounds for length 4 p0=1
// offset 20 30 40 50
// offset sipush 1000 1000
// wide array 21 24 27 30
// wide index 9 9 9 9
// long 0 100 200
// byte -56
// value 7 7
// HOTSPOT_EXPECTED_END
public class L7W77ArrayOperand {
    static int SF = 5;

    static final class Init {
        static final int[] T = new int[4];

        static {
            // Locals: j 0, i 1; `getstatic T; iload_0; dup2; iaload;
            // getstatic SF; iadd; iastore` (the stack-top entry) in <clinit>.
            int j = 0;
            for (int i = 0; i < 200; i++) {
                j = i & 3;
                T[j] += SF;
            }
        }
    }

    // Locals: a 0, b 1, j 2, k 3: `aload_0; iload_2; dup2; iaload; aload_1;
    // iload_3; iaload; iadd; iastore`.
    static void addAt(int[] a, int[] b, int j, int k) {
        a[j] += b[k];
    }

    // Locals: p 0, j 1: `aload_0; iload_1; dup2; iaload; aload_0; iload_1;
    // iconst_1; isub; iaload; iadd; iastore`.
    static void prefixAt(int[] p, int j) {
        p[j] += p[j - 1];
    }

    // Locals: h 0, w 1, j 2: `... aload_1; iload_2; iconst_1; iadd; iaload ...`.
    static void offsetAt(int[] h, int[] w, int j) {
        h[j] += w[j + 1];
    }

    // `... aload_1; iload_2; sipush 300; iadd; iaload ...`.
    static void offsetFar(int[] h, int[] w, int j) {
        h[j] += w[j + 300];
    }

    // Locals: acc 0, j 1, pad 2, pad2 3, x 4: the operand array is `aload 4`.
    static void wideArray(int[] acc, int j, int pad, int pad2, int[] x) {
        acc[j] += x[j];
    }

    // Locals: a 0, b 1, j 2, pad 3, k 4: the operand index is `iload 4`.
    static void wideIndex(int[] a, int[] b, int j, int pad, int k) {
        a[j] += b[k];
    }

    // Locals: la 0, lb 1, j 2: `la[j] += lb[j - 1]`.
    static void longAt(long[] la, long[] lb, int j) {
        la[j] += lb[j - 1];
    }

    // `bs[j] += bs[j - 1]`, narrowed by i2b.
    static void byteAt(byte[] bs, int j) {
        bs[j] += bs[j - 1];
    }

    // The value form: `return a[j] += b[j + 1]`.
    static int valueAt(int[] a, int[] b, int j) {
        return a[j] += b[j + 1];
    }

    public static void main(String[] args) {
        int[] t = Init.T;
        System.out.println("clinit " + t[0] + " " + t[1] + " " + t[2] + " " + t[3]);

        // 100 calls whose operand index is out of range (more than the
        // 64-miss budget), then calls that hit at the same site.
        int[] a = new int[4];
        int[] shortB = {3, 4};
        int thrown = 0;
        String msg = "";
        for (int n = 0; n < 100; n++) {
            try {
                addAt(a, shortB, 1, 5);
            } catch (ArrayIndexOutOfBoundsException e) {
                thrown++;
                msg = e.getMessage();
            }
        }
        System.out.println("misses " + thrown + " " + msg + " a=" + a[1]);
        int[] b = {10, 10, 10, 10};
        for (int n = 0; n < 4; n++) {
            addAt(a, b, 1, n);
        }
        System.out.println("after misses " + a[1]);

        int[] p = {1, 2, 3, 4};
        for (int j = 1; j < 4; j++) {
            prefixAt(p, j);
        }
        System.out.println("prefix " + p[0] + " " + p[1] + " " + p[2] + " " + p[3]);
        int oob = 0;
        String oobMsg = "";
        for (int n = 0; n < 100; n++) {
            try {
                prefixAt(p, 0);
            } catch (ArrayIndexOutOfBoundsException e) {
                oob++;
                oobMsg = e.getMessage();
            }
        }
        System.out.println("prefix oob " + oob + " " + oobMsg + " p0=" + p[0]);

        int[] h = new int[4];
        int[] w = {10, 20, 30, 40, 50};
        for (int j = 0; j < 4; j++) {
            offsetAt(h, w, j);
        }
        System.out.println("offset " + h[0] + " " + h[1] + " " + h[2] + " " + h[3]);
        int[] far = new int[302];
        far[300] = 1000;
        far[301] = 1000;
        int[] hf = new int[2];
        offsetFar(hf, far, 0);
        offsetFar(hf, far, 1);
        System.out.println("offset sipush " + hf[0] + " " + hf[1]);

        int[] acc = new int[4];
        int[] x = {7, 8, 9, 10};
        for (int r = 0; r < 3; r++) {
            for (int j = 0; j < 4; j++) {
                wideArray(acc, j, 0, 0, x);
            }
        }
        System.out.println("wide array " + acc[0] + " " + acc[1] + " " + acc[2] + " " + acc[3]);

        int[] wi = new int[4];
        int[] nines = {9, 9, 9, 9};
        for (int j = 0; j < 4; j++) {
            wideIndex(wi, nines, j, 0, 3 - j);
        }
        System.out.println("wide index " + wi[0] + " " + wi[1] + " " + wi[2] + " " + wi[3]);

        long[] la = new long[3];
        long[] lb = {100L, 200L, 300L};
        longAt(la, lb, 1);
        longAt(la, lb, 2);
        System.out.println("long " + la[0] + " " + la[1] + " " + la[2]);

        byte[] bs = {100, 100};
        byteAt(bs, 1);
        System.out.println("byte " + bs[1]);

        int[] va = {0, 0};
        int[] vb = {0, 7, 7};
        int v0 = valueAt(va, vb, 0);
        System.out.println("value " + v0 + " " + va[0]);
    }
}
