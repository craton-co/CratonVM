// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L7: the fused array element
// increment's per-method decline memo
// (CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO, default off). A compound
// assignment whose group the decoder refuses on its bytes (`a[j] += SF` with
// a non-final static, `a[j] += b[k]`, `a[j] = a[j] + b[j]`, `la[j] += i`
// with an int widened by `i2l`, `f[j] += 0.5f`) is remembered per method and
// pc, and the dispatch loop stops calling the fused helper there. No
// behaviour change: every line is the same with the switch off and on, with
// and without --nojit.
//
// Checked: a remembered site still computes the unfused result and still
// throws at its own `*aload` (AIOOBE of the element, AIOOBE of the operand
// array with the element left unchanged, NPE of a null array) after the
// memo is set; a declining site next to a fused one in the same method
// (`a[j] += SF; a[j]++`); a site whose first executions decline per
// execution (null array, index out of range), which must not be remembered
// and still computes the fused result afterwards; the stack-top entry
// (`this.counts[j] += SF`). Every array and index sits in locals 0..3 so the
// fused entries are what run.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// static 3000
// static oob Index 8 out of bounds for length 8
// static npe java.lang.NullPointerException
// array operand 3600
// operand oob Index 8 out of bounds for length 8 a=3600
// nodup mix 10 80
// mixed 400
// inc npe java.lang.NullPointerException
// inc oob Index 9 out of bounds for length 8
// inc after declines 50
// field static 30
// long widen 4950
// float 5.0
// HOTSPOT_EXPECTED_END
public class L7W75ArrayDeclineMemo {
    static int SF = 3;
    static final int[] B = {1, 2, 3, 4, 5, 6, 7, 8};

    int[] counts = new int[8];

    static void p(String s) {
        System.out.println(s);
    }

    // Locals: a 0, j 1, n 2, i 3.
    static int staticOperand(int[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] += SF;
        }
        return a[j];
    }

    // Locals: a 0, j 1, b 2, k 3.
    static void arrOperand(int[] a, int j, int[] b, int k) {
        a[j] += b[k];
    }

    // Locals: a 0, b 1, j 2, i 3.
    static void noDupMix(int[] a, int[] b) {
        int j = 0;
        for (int i = 0; i < 80; i++) {
            j = i & 7;
            a[j] = a[j] + b[j];
        }
    }

    // Locals: a 0, j 1, n 2, i 3.
    static void mixed(int[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] += SF;
            a[j]++;
        }
    }

    static void inc(int[] a, int j) {
        a[j]++;
    }

    // Locals: this 0, j 1, n 2, i 3.
    void fieldStatic(int j, int n) {
        for (int i = 0; i < n; i++) {
            counts[j] += SF;
        }
    }

    // Locals: la 0, j 1, n 2, i 3.
    static long longWiden(long[] la, int j, int n) {
        for (int i = 0; i < n; i++) {
            la[j] += i;
        }
        return la[j];
    }

    // Locals: f 0, j 1, n 2, i 3.
    static float floatAdd(float[] f, int j, int n) {
        for (int i = 0; i < n; i++) {
            f[j] += 0.5f;
        }
        return f[j];
    }

    public static void main(String[] args) {
        int[] a = new int[8];
        p("static " + staticOperand(a, 2, 1000));
        try {
            staticOperand(a, 8, 1);
            p("static oob none");
        } catch (ArrayIndexOutOfBoundsException e) {
            p("static oob " + e.getMessage());
        }
        try {
            staticOperand(null, 0, 1);
            p("static npe none");
        } catch (NullPointerException e) {
            p("static npe " + e.getClass().getName());
        }

        int[] c = new int[8];
        for (int i = 0; i < 800; i++) {
            arrOperand(c, 3, B, i & 7);
        }
        p("array operand " + c[3]);
        try {
            arrOperand(c, 3, B, 8);
            p("operand oob none");
        } catch (ArrayIndexOutOfBoundsException e) {
            p("operand oob " + e.getMessage() + " a=" + c[3]);
        }

        int[] d = new int[8];
        noDupMix(d, B);
        p("nodup mix " + d[0] + " " + d[7]);

        int[] m = new int[8];
        mixed(m, 5, 100);
        p("mixed " + m[5]);

        int[] e8 = new int[8];
        try {
            inc(null, 1);
            p("inc npe none");
        } catch (NullPointerException e) {
            p("inc npe " + e.getClass().getName());
        }
        try {
            inc(e8, 9);
            p("inc oob none");
        } catch (ArrayIndexOutOfBoundsException e) {
            p("inc oob " + e.getMessage());
        }
        for (int i = 0; i < 50; i++) {
            inc(e8, 1);
        }
        p("inc after declines " + e8[1]);

        L7W75ArrayDeclineMemo t = new L7W75ArrayDeclineMemo();
        t.fieldStatic(4, 10);
        p("field static " + t.counts[4]);

        p("long widen " + longWiden(new long[4], 1, 100));
        p("float " + floatAdd(new float[4], 2, 10));
    }
}
