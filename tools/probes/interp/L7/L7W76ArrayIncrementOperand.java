// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L7: the fused array element
// increment's two deferred operands,
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_ARRAY_OPERAND   `a[j] += b[k]`,
//       `a[j] = a[j] * b[j]` (another element, locals 0..3; default off)
//   CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_STATIC_OPERAND  `a[j] += SF`
//       (a getstatic answered from a filled quickened site; default off).
// No behaviour change: every line is the same with each switch off and on,
// with and without --nojit, and with CRATONVM_INTERP_FUSED_ARRAY_DECLINE_MEMO
// off and on.
//
// Checked: an int, a volatile int, a byte (narrowed by i2b), a long and a
// shift-count static; a static whose class the first execution initialises
// (the cold-site miss: "holder init" prints once, before the result); a zero
// static divisor (ArithmeticException, element unchanged); the element's
// AIOOBE and a null array's NPE ahead of the operand; an int, byte, char and
// long element operand; the operand's AIOOBE and NPE with the element left
// unchanged; one element twice (`a[j] += a[j]`); the value form
// (`return a[j] += b[k]`); the no-dup form; a short element narrowed by i2s.
// Every array and index sits in locals 0..3 so the fused entries are what
// run.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// static 3000
// volatile 500
// byte 44
// long 3298534883328
// holder init
// holder 70
// shift 4096
// div / by zero a=10
// static oob Index 4 out of bounds for length 4
// static npe java.lang.NullPointerException
// array operand 3600
// operand oob Index 8 out of bounds for length 8 a=3600
// operand npe java.lang.NullPointerException a=3600
// element oob Index 4 out of bounds for length 4
// byte operand 1
// char operand -65600
// long operand -9223372036854775808
// alias 40
// value 21 21
// nodup 42
// short -32768
// HOTSPOT_EXPECTED_END
public class L7W76ArrayIncrementOperand {
    static int SF = 3;
    static volatile int VF = 5;
    static byte BS = 100;
    static long LS = 1L << 40;
    static int Z = 0;
    static final int[] B = {1, 2, 3, 4, 5, 6, 7, 8};

    static class Holder {
        static int K;

        static {
            K = 7;
            System.out.println("holder init");
        }
    }

    static void p(String s) {
        System.out.println(s);
    }

    // Static operand rows. Locals: a 0, j 1, n 2, i 3.
    static int staticOp(int[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] += SF;
        }
        return a[j];
    }

    static int volatileOp(int[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] += VF;
        }
        return a[j];
    }

    static int byteOp(byte[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] += BS;
        }
        return a[j];
    }

    static long longOp(long[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] += LS;
        }
        return a[j];
    }

    static int holderOp(int[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] += Holder.K;
        }
        return a[j];
    }

    static int shlOp(int[] a, int j, int n) {
        for (int i = 0; i < n; i++) {
            a[j] <<= SF;
        }
        return a[j];
    }

    // Locals: a 0, j 1.
    static void divOp(int[] a, int j) {
        a[j] /= Z;
    }

    // Element operand rows. Locals: a 0, j 1, b 2, k 3.
    static void arrOp(int[] a, int j, int[] b, int k) {
        a[j] += b[k];
    }

    static void byteArrOp(int[] a, int j, byte[] b, int k) {
        a[j] += b[k];
    }

    static void charArrOp(int[] a, int j, char[] b, int k) {
        a[j] -= b[k];
    }

    static void longArrOp(long[] a, int j, long[] b, int k) {
        a[j] += b[k];
    }

    static void shortArrOp(short[] a, int j, short[] b, int k) {
        a[j] += b[k];
    }

    static int valueOp(int[] a, int j, int[] b, int k) {
        return a[j] += b[k];
    }

    // Locals: a 0, j 1.
    static void aliasOp(int[] a, int j) {
        a[j] += a[j];
    }

    // Locals: a 0, j 1, b 2.
    static void noDupOp(int[] a, int j, int[] b) {
        a[j] = a[j] * b[j];
    }

    public static void main(String[] args) {
        int[] a = new int[4];
        p("static " + staticOp(a, 1, 1000));
        p("volatile " + volatileOp(new int[4], 2, 100));
        p("byte " + byteOp(new byte[4], 0, 3));
        p("long " + longOp(new long[2], 1, 3));
        p("holder " + holderOp(new int[4], 3, 10));
        p("shift " + shlOp(new int[] {1, 1, 1, 1}, 0, 4));
        int[] ten = {10};
        try {
            divOp(ten, 0);
            p("div none");
        } catch (ArithmeticException e) {
            p("div " + e.getMessage() + " a=" + ten[0]);
        }
        try {
            staticOp(a, 4, 1);
            p("static oob none");
        } catch (ArrayIndexOutOfBoundsException e) {
            p("static oob " + e.getMessage());
        }
        try {
            staticOp(null, 0, 1);
            p("static npe none");
        } catch (NullPointerException e) {
            p("static npe " + e.getClass().getName());
        }

        int[] c = new int[4];
        for (int i = 0; i < 800; i++) {
            arrOp(c, 3, B, i & 7);
        }
        p("array operand " + c[3]);
        try {
            arrOp(c, 3, B, 8);
            p("operand oob none");
        } catch (ArrayIndexOutOfBoundsException e) {
            p("operand oob " + e.getMessage() + " a=" + c[3]);
        }
        try {
            arrOp(c, 3, null, 0);
            p("operand npe none");
        } catch (NullPointerException e) {
            p("operand npe " + e.getClass().getName() + " a=" + c[3]);
        }
        try {
            arrOp(c, 4, B, 0);
            p("element oob none");
        } catch (ArrayIndexOutOfBoundsException e) {
            p("element oob " + e.getMessage());
        }

        byte[] bb = {-3, 127, -128, 5};
        int[] d = new int[1];
        for (int k = 0; k < bb.length; k++) {
            byteArrOp(d, 0, bb, k);
        }
        p("byte operand " + d[0]);

        char[] cc = {'A', (char) 0xFFFF};
        int[] e = new int[1];
        charArrOp(e, 0, cc, 0);
        charArrOp(e, 0, cc, 1);
        p("char operand " + e[0]);

        long[] la = {Long.MAX_VALUE, 0L};
        long[] lb = {1L, 2L};
        longArrOp(la, 0, lb, 0);
        p("long operand " + la[0]);

        int[] f = {5, 0};
        for (int i = 0; i < 3; i++) {
            aliasOp(f, 0);
        }
        p("alias " + f[0]);

        int[] g = {1, 0};
        int[] h = {10, 20};
        int v = valueOp(g, 0, h, 1);
        p("value " + v + " " + g[0]);

        int[] m = {2, 3, 4, 5};
        int[] three = {3, 3, 3, 3};
        for (int j = 0; j < m.length; j++) {
            noDupOp(m, j, three);
        }
        p("nodup " + (m[0] + m[1] + m[2] + m[3]));

        short[] s1 = {32767};
        short[] s2 = {1};
        shortArrOp(s1, 0, s2, 0);
        p("short " + s1[0]);
    }
}
