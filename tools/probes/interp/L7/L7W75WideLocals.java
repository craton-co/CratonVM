// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L7: the element read `a[i]` with a
// local past 3 (CRATONVM_INTERP_FUSED_WIDE_LOCALS, default off):
// `aload_N; iload n; <x>aload` (the for-each loop's hidden index) and
// `aload n; iload{_M, n}; <x>aload`. No behaviour change: every line is the
// same with the switch off and on, with and without --nojit.
//
// Checked: every element type (`iaload`, `laload`, `baload` sign-extended,
// `caload` zero-extended, `saload`, a `boolean[]` through `baload`, `faload`,
// `daload`, `aaload`) with the array in locals 0..3 and past 3 and the
// index past 3; the AIOOBE of an index out of range either side and the NPE
// of a null array, thrown at the `*aload` with nothing read; a for-each
// hash over a `byte[]`; a sum whose array and index are both past 3; a sum
// whose array is past 3 and whose index is in 0..3.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// elems -2 -9223372036854775808 -128 z -32768 true 1.5 -2.25 y
// oob Index 3 out of bounds for length 3
// neg Index -1 out of bounds for length 3
// npe java.lang.NullPointerException
// foreach 1223
// wideboth 15
// widearr 72
// HOTSPOT_EXPECTED_END
public class L7W75WideLocals {
    static void p(String s) {
        System.out.println(s);
    }

    // Locals: ia 0, la 1, ba 2, ca 3, sa 4, za 5, fa 6, da 7, oa 8, i 9.
    static String elems(int[] ia, long[] la, byte[] ba, char[] ca, short[] sa, boolean[] za,
            float[] fa, double[] da, Object[] oa, int i) {
        return ia[i] + " " + la[i] + " " + ba[i] + " " + ca[i] + " " + sa[i] + " " + za[i]
                + " " + fa[i] + " " + da[i] + " " + oa[i];
    }

    // Locals: x 0, b 1, h 2, then the for-each's array 3, length 4, index 5,
    // element 6.
    static int forEach(int x, byte[] b) {
        int h = x;
        for (byte v : b) {
            h = 31 * h + (v & 0xff);
        }
        return h;
    }

    // Locals: a 0, n 1, x 2, y 3, b 4, s 5, i 6.
    static int wideBoth(int[] a, int n, int x, int y) {
        int[] b = a;
        int s = x + y;
        for (int i = 0; i < n; i++) {
            s += b[i];
        }
        return s;
    }

    // Locals: j 0, n 1, s 2, i 3, a 4.
    static int wideArr(int j, int n, int s, int i, int[] a) {
        for (i = 0; i < n; i++) {
            j = i & 7;
            s += a[j];
        }
        return s;
    }

    public static void main(String[] args) {
        int[] ia = {1, -2, 3};
        long[] la = {0, Long.MIN_VALUE, 1};
        byte[] ba = {0, (byte) -128, 1};
        char[] ca = {'a', 'z', 'b'};
        short[] sa = {0, (short) -32768, 1};
        boolean[] za = {false, true, false};
        float[] fa = {0f, 1.5f, 2f};
        double[] da = {0, -2.25, 1};
        Object[] oa = {"x", "y", "z"};
        p("elems " + elems(ia, la, ba, ca, sa, za, fa, da, oa, 1));
        try {
            p("oob none " + elems(ia, la, ba, ca, sa, za, fa, da, oa, 3));
        } catch (ArrayIndexOutOfBoundsException e) {
            p("oob " + e.getMessage());
        }
        try {
            p("neg none " + elems(ia, la, ba, ca, sa, za, fa, da, oa, -1));
        } catch (ArrayIndexOutOfBoundsException e) {
            p("neg " + e.getMessage());
        }
        try {
            p("npe none " + elems(null, la, ba, ca, sa, za, fa, da, oa, 1));
        } catch (NullPointerException e) {
            p("npe " + e.getClass().getName());
        }
        p("foreach " + forEach(0, new byte[] {1, 2, (byte) 200}));
        p("wideboth " + wideBoth(new int[] {1, 2, 3, 4, 5}, 5, 0, 0));
        p("widearr " + wideArr(0, 16, 0, 0, new int[] {1, 2, 3, 4, 5, 6, 7, 8}));
    }
}
