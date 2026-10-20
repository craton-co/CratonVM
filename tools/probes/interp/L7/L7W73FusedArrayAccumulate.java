// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 73, lane L7: the fused array element accumulate
// `iload_S; aload_N; iload_M; iaload | baload | caload | saload; iadd |
// isub | imul | iand | ior | ixor; istore_Z` (`s += a[i]`, `h ^= b[k]`,
// `t = s + a[i]`) under CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE (default off;
// proposal i72-L7-proposal-fuse-the-array-element-accumulate-20261105).
// No behaviour change: every line is the same with the switch off and on,
// with and without --nojit.
//
// Every accumulator, array and index sits in locals 0..3 where the fused
// shape needs it, so the fused path is what runs; the exception lines check
// that a decline lets the unfused `*aload` throw with HotSpot's message
// (and that the accumulator keeps the value it had before the throwing
// iteration).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// int sum=2147483644 sub=-2147483644 mul=-30 and=0 or=-1 xor=2147483640
// byte sum=4 xor=4 char sum=98401 or=65535 short sum=6 sub=-6
// other s=2147483643 t=2147483647
// loop sum=499500 byteSum=2284 charXor=36120
// aioobe java.lang.ArrayIndexOutOfBoundsException Index 5 out of bounds for length 5 s=2147483644
// aioobe java.lang.ArrayIndexOutOfBoundsException Index -1 out of bounds for length 5 s=1
// npe java.lang.NullPointerException s=0
// empty 0 0
// HOTSPOT_EXPECTED_END
public class L7W73FusedArrayAccumulate {
    static final int[] A = {1, -2, 3, 0x7fffffff, -5};
    static final byte[] B = {1, -1, 127, -128, 5};
    static final char[] C = {'a', (char) 0xffff, 1, (char) 0x8000};
    static final short[] S = {1, -1, 32767, -32768, 7};

    // Locals: a = 0, s = 1, i = 2 (`iload_1; aload_0; iload_2; <x>aload;
    // <op>; istore_1`).
    static int sumInt(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int subInt(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s -= a[i];
        }
        return s;
    }

    static int mulInt(int[] a) {
        int s = 1;
        for (int i = 0; i < a.length; i++) {
            s *= a[i];
        }
        return s;
    }

    static int andInt(int[] a) {
        int s = -1;
        for (int i = 0; i < a.length; i++) {
            s &= a[i];
        }
        return s;
    }

    static int orInt(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s |= a[i];
        }
        return s;
    }

    static int xorInt(int[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s ^= a[i];
        }
        return s;
    }

    static int sumByte(byte[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int xorByte(byte[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s ^= a[i];
        }
        return s;
    }

    static int sumChar(char[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int orChar(char[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s |= a[i];
        }
        return s;
    }

    static int sumShort(short[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static int subShort(short[] a) {
        int s = 0;
        for (int i = 0; i < a.length; i++) {
            s -= a[i];
        }
        return s;
    }

    // Locals: a = 0, s = 1, i = 2, t = 3 (`t = s + a[i]` stores another
    // local: `iload_1; aload_0; iload_2; iaload; iadd; istore_3`).
    static String other(int[] a) {
        int s = 7;
        int i = 0;
        int t = 0;
        for (i = 0; i < a.length; i++) {
            t = s + a[i];
            s = t ^ i;
        }
        return "s=" + s + " t=" + t;
    }

    static int out;

    // Locals: a = 0, n = 1, s = 2, i = 3 (`iload_2; aload_0; iload_3;
    // iaload; iadd; istore_2`); the accumulator is published before a throw.
    static int sumUpTo(int[] a, int n) {
        int s = 0;
        try {
            for (int i = 0; i < n; i++) {
                s += a[i];
            }
        } finally {
            out = s;
        }
        return s;
    }

    // Locals: a = 0, j = 1, s = 2, k = 3.
    static int sumAt(int[] a, int j) {
        int s = 1;
        int k = j;
        try {
            s += a[k];
        } finally {
            out = s;
        }
        return s;
    }

    static String loops() {
        int[] a = new int[1000];
        byte[] b = new byte[1000];
        char[] c = new char[1000];
        for (int i = 0; i < 1000; i++) {
            a[i] = i;
            b[i] = (byte) ((i & 0x80) | (0x7f & ~i));
            c[i] = (char) (i * 7919);
        }
        int sum = 0;
        int byteSum = 0;
        int charXor = 0;
        for (int r = 0; r < 3; r++) {
            sum = sumInt(a);
            byteSum = sumByte(b);
            charXor = xorChars(c);
        }
        return "loop sum=" + sum + " byteSum=" + byteSum + " charXor=" + charXor;
    }

    static int xorChars(char[] c) {
        int s = 0;
        for (int i = 0; i < c.length; i++) {
            s ^= c[i];
        }
        return s;
    }

    public static void main(String[] args) {
        System.out.println("int sum=" + sumInt(A) + " sub=" + subInt(A) + " mul=" + mulInt(A)
                + " and=" + andInt(A) + " or=" + orInt(A) + " xor=" + xorInt(A));
        System.out.println("byte sum=" + sumByte(B) + " xor=" + xorByte(B) + " char sum=" + sumChar(C)
                + " or=" + orChar(C) + " short sum=" + sumShort(S) + " sub=" + subShort(S));
        System.out.println("other " + other(A));
        System.out.println(loops());
        try {
            sumUpTo(A, 6);
            System.out.println("aioobe none");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("aioobe " + e.getClass().getName() + " " + e.getMessage() + " s=" + out);
        }
        try {
            sumAt(A, -1);
            System.out.println("aioobe none");
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("aioobe " + e.getClass().getName() + " " + e.getMessage() + " s=" + out);
        }
        try {
            sumUpTo(null, 1);
            System.out.println("npe none");
        } catch (NullPointerException e) {
            System.out.println("npe " + e.getClass().getName() + " s=" + out);
        }
        System.out.println("empty " + sumUpTo(A, 0) + " " + sumInt(new int[0]));
    }
}
