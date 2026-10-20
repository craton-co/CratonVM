// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L7: the fused array element increment
// widened to the shifts and divisions (`a[j] <<= k`, `a[j] >>>= k`,
// `a[j] /= k`, `a[j] %= k`, their narrow and long twins) under
// CRATONVM_INTERP_FUSED_ARRAY_INCREMENT_SHIFT_DIV, and the update written
// without a compound operator (`a[j] = a[j] + k`, no `dup2`) under
// CRATONVM_INTERP_FUSED_ARRAY_UPDATE_NO_DUP. Both default off.
// No behaviour change: every line is the same with each switch off and on,
// with and without --nojit.
//
// Checked: the JVM's shift count masks (low five / six bits, negative and
// oversized counts from a local), `>>>` of a negative narrow element (the
// int-wide shift, then the element's own conversion), `MIN_VALUE / -1` and
// `% -1`, a zero divisor (the unfused `idiv` / `ldiv` throws
// `ArithmeticException: / by zero` at its own line and the element is
// unchanged), the value form (`x = (a[j] <<= 2)`), the no-dup group's value
// form and narrow / long elements, a no-dup prefix whose group is not an
// update of the same element (declined, computed unfused), and the
// AIOOBE / NPE of a no-dup group (thrown by the first unfused `*aload`).
// Every array and index sits in locals 0..3 so the fused entries are what
// run.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// ishl 1 = 6
// ishl 33 = 6
// ishl -1 = -2147483648
// ishr = -4
// ishr 33 = -4
// iushr = 2147483647
// idiv = -2
// irem = -1
// idiv min/-1 = -2147483648
// irem min%-1 = 0
// pre value = 20 20
// neighbours = 0 0 0
// byte shl = -128
// byte ushr = -64
// char shr = 32767
// char shl = 32768
// short div = -16384
// short div -1 = -32768
// short rem = 6
// lshl 1 = 2
// lshl 65 = 2
// lshr = -2
// lushr = 9223372036854775807
// ldiv min/-1 = -9223372036854775808
// lrem min%-1 = 0
// ldiv 1 = -9
// pre value = 40 40
// idiv0 / by zero at divZero:182 a=6
// irem0 / by zero at divZero:188 a=6
// ldiv0 / by zero at divZeroLong:200 la=6
// lrem0 / by zero at divZeroLong:206 la=6
// nodup = 16
// nodup shl = 64
// nodup div = 21
// nodup value = 22 22
// nodup cross = 23 22
// nodup decline = -1
// nodup tail = 5
// nodup byte = -128
// nodup char = 65535
// nodup long = 1
// nodup long shl = 2
// nodup aioobe Index 8 out of bounds for length 8 at noDupOob:259
// nodup npe java.lang.NullPointerException at noDupNull:268
// loop = 47720
// HOTSPOT_EXPECTED_END
public class L7W74ArrayShiftDivNoDup {
    static void p(String s) {
        System.out.println(s);
    }

    static void ints() {
        int[] a = new int[4];
        int j = 1;
        int c = 33;
        int m = -1;
        a[j] = 3;
        a[j] <<= 1;
        p("ishl 1 = " + a[j]);
        a[j] = 3;
        a[j] <<= c;
        p("ishl 33 = " + a[j]);
        a[j] = 1;
        a[j] <<= m;
        p("ishl -1 = " + a[j]);
        a[j] = -7;
        a[j] >>= 1;
        p("ishr = " + a[j]);
        a[j] = -8;
        a[j] >>= c;
        p("ishr 33 = " + a[j]);
        a[j] = -2;
        a[j] >>>= 1;
        p("iushr = " + a[j]);
        a[j] = -7;
        a[j] /= 3;
        p("idiv = " + a[j]);
        a[j] = -7;
        a[j] %= 3;
        p("irem = " + a[j]);
        a[j] = Integer.MIN_VALUE;
        a[j] /= m;
        p("idiv min/-1 = " + a[j]);
        a[j] = Integer.MIN_VALUE;
        a[j] %= m;
        p("irem min%-1 = " + a[j]);
        a[j] = 5;
        int x = (a[j] <<= 2);
        p("pre value = " + x + " " + a[j]);
        p("neighbours = " + a[0] + " " + a[2] + " " + a[3]);
    }

    static void narrow() {
        byte[] b = new byte[2];
        char[] ch = new char[2];
        short[] sh = new short[2];
        int j = 1;
        b[j] = 64;
        b[j] <<= 1;
        p("byte shl = " + b[j]);
        b[j] = -128;
        b[j] >>>= 1;
        p("byte ushr = " + b[j]);
        ch[j] = (char) 0xffff;
        ch[j] >>= 1;
        p("char shr = " + (int) ch[j]);
        ch[j] = 7;
        ch[j] <<= 15;
        p("char shl = " + (int) ch[j]);
        sh[j] = -32768;
        sh[j] /= 2;
        p("short div = " + sh[j]);
        sh[j] = -32768;
        sh[j] /= -1;
        p("short div -1 = " + sh[j]);
        sh[j] = 1000;
        sh[j] %= 7;
        p("short rem = " + sh[j]);
    }

    static void longs() {
        long[] la = new long[2];
        int j = 1;
        int c = 65;
        long m = -1;
        la[j] = 1;
        la[j] <<= 1;
        p("lshl 1 = " + la[j]);
        la[j] = 1;
        la[j] <<= c;
        p("lshl 65 = " + la[j]);
        la[j] = -3;
        la[j] >>= 1;
        p("lshr = " + la[j]);
        la[j] = -2;
        la[j] >>>= 1;
        p("lushr = " + la[j]);
        la[j] = Long.MIN_VALUE;
        la[j] /= m;
        p("ldiv min/-1 = " + la[j]);
        la[j] = Long.MIN_VALUE;
        la[j] %= m;
        p("lrem min%-1 = " + la[j]);
        la[j] = -9;
        la[j] /= 1;
        p("ldiv 1 = " + la[j]);
        la[j] = 5;
        long x = (la[j] <<= 3);
        p("pre value = " + x + " " + la[j]);
    }

    static void divZero() {
        int[] a = {5, 6};
        int j = 1;
        int z = 0;
        try {
            a[j] /= z;
            p("idiv0 no throw");
        } catch (ArithmeticException e) {
            p("idiv0 " + e.getMessage() + " at " + where(e) + " a=" + a[j]);
        }
        try {
            a[j] %= z;
            p("irem0 no throw");
        } catch (ArithmeticException e) {
            p("irem0 " + e.getMessage() + " at " + where(e) + " a=" + a[j]);
        }
    }

    static void divZeroLong() {
        long[] la = {5, 6};
        int j = 1;
        long z = 0;
        try {
            la[j] /= z;
            p("ldiv0 no throw");
        } catch (ArithmeticException e) {
            p("ldiv0 " + e.getMessage() + " at " + where(e) + " la=" + la[j]);
        }
        try {
            la[j] %= z;
            p("lrem0 no throw");
        } catch (ArithmeticException e) {
            p("lrem0 " + e.getMessage() + " at " + where(e) + " la=" + la[j]);
        }
    }

    static String where(Throwable e) {
        StackTraceElement top = e.getStackTrace()[0];
        return top.getMethodName() + ":" + top.getLineNumber();
    }

    static void noDup() {
        int[] a = new int[8];
        int j = 3;
        int k = 5;
        a[j] = a[j] + 1;
        a[j] = a[j] + k;
        a[j] = a[j] * 3;
        a[j] = a[j] - 2;
        p("nodup = " + a[j]);
        a[j] = a[j] << 2;
        p("nodup shl = " + a[j]);
        a[j] = a[j] / 3;
        p("nodup div = " + a[j]);
        int x = (a[j] = a[j] + 1);
        p("nodup value = " + x + " " + a[j]);
        a[k] = a[j] + 1;
        p("nodup cross = " + a[k] + " " + a[j]);
        a[j] = a[j] - a[k];
        p("nodup decline = " + a[j]);
        a[j] = a[j] / 2 + k;
        p("nodup tail = " + a[j]);
    }

    static void noDupNarrow() {
        byte[] b = new byte[2];
        char[] ch = new char[2];
        long[] la = new long[2];
        int j = 1;
        b[j] = 127;
        b[j] = (byte) (b[j] + 1);
        p("nodup byte = " + b[j]);
        ch[j] = (char) (ch[j] - 1);
        p("nodup char = " + (int) ch[j]);
        la[j] = la[j] + 1;
        p("nodup long = " + la[j]);
        la[j] = la[j] << 1;
        p("nodup long shl = " + la[j]);
    }

    static void noDupOob(int[] a, int j) {
        try {
            a[j] = a[j] + 1;
            p("nodup aioobe no throw");
        } catch (ArrayIndexOutOfBoundsException e) {
            p("nodup aioobe " + e.getMessage() + " at " + where(e));
        }
    }

    static void noDupNull(int[] n, int j) {
        try {
            n[j] = n[j] + 1;
            p("nodup npe no throw");
        } catch (NullPointerException e) {
            p("nodup npe " + e.getClass().getName() + " at " + where(e));
        }
    }

    /** Every group in one loop, 1000 iterations, so the warm path decides. */
    static int loop() {
        int[] a = new int[8];
        int j = 0;
        for (int i = 0; i < 1000; i++) {
            j = i & 7;
            a[j] <<= 1;
            a[j] += i;
            a[j] %= 1009;
            a[j] = a[j] + 3;
            a[j] >>= 1;
            a[j] = a[j] * 5;
        }
        int s = 0;
        for (int k = 0; k < a.length; k++) {
            s += a[k] * (k + 1);
        }
        return s;
    }

    public static void main(String[] args) {
        ints();
        narrow();
        longs();
        divZero();
        divZeroLong();
        noDup();
        noDupNarrow();
        noDupOob(new int[8], 8);
        noDupNull(null, 0);
        p("loop = " + loop());
    }
}
