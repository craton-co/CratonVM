// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane fuzz2: ALLOC21-4's unused-array elimination
// (`CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM`: `find_scalar_replacements` admits a
// constant-length primitive array of 9..=64 elements -- past
// `MAX_SCALAR_ARRAY_LEN` (8) -- when every use edge is a dead node), with the
// length fold that makes `.length` reads disappear first
// (`CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD`).
//
// Risk targeted: "no live use" is decided on IR uses, but a frame state
// still names the array. A branch the profile never saw becomes a trap whose
// frame state is the array's only reference; at the deopt the array must be
// MATERIALIZED with its full length (16, 40, 64 elements -- past the old
// 8-slot recipes), its zero contents, the right class, and ONE identity for
// two locals that name it. Also the cap edges (8, 9, 64, 65), every primitive
// element type, a length from a static final, the same rows inside an OSR
// loop, and a second deopt after a re-warm.
//
// Run:
//   javac -d out R21Fz2ArrElim1.java
//   java -Xmx256m -cp out R21Fz2ArrElim1                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz2ArrElim1
//   CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1 cratonvm -Xmx256m -cp out R21Fz2ArrElim1
//   CRATONVM_JIT_EA_UNUSED_ARRAY_ELIM=1 CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD=1 cratonvm -Xmx256m -cp out R21Fz2ArrElim1
//   ... + CRATONVM_DBG_SCALAR_NEW=1 CRATONVM_DBG_SCALAR_DEOPT=1 (stderr: replaced / materialized)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

import java.util.Arrays;

public class R21Fz2ArrElim1 {
    static final int WARM = 200_000;
    static final int RARE = 9_999_991;
    static final int N40 = 40;

    static long e0(int x) {
        int[] a = new int[40];
        int[] alias = a;
        long r = x + a.length;
        if (x == RARE) {
            alias[39] = 7;
            r = a.length * 1000L + a[39] + a[0] + (alias == a ? 100_000 : 0)
                    + (a.getClass() == int[].class ? 1_000_000 : 0);
        }
        return r;
    }

    static long e1(int x) {
        long[] a = new long[64];
        if (x == RARE) {
            long s = 0;
            for (long v : a) {
                s += v;
            }
            a[63] = Long.MIN_VALUE;
            return s + a.length + (a[63] >>> 60) + a.getClass().getName().length();
        }
        return x - a.length;
    }

    static long e2(int x) {
        byte[] b = new byte[16];
        short[] s = new short[9];
        char[] c = new char[N40];
        boolean[] z = new boolean[64];
        long r = b.length + s.length + c.length + z.length + x;
        if (x == RARE) {
            b[15] = -1;
            s[8] = Short.MIN_VALUE;
            c[39] = '￿';
            z[63] = true;
            r = b[15] + s[8] + c[39] + (z[63] ? 1 : 0) + b.length * 10_000L + s.length * 100L + c.length + z.length
                    + (b[0] + s[0] + c[0] + (z[0] ? 7 : 0));
        }
        return r;
    }

    static long e3(int x) {
        float[] f = new float[33];
        double[] d = new double[64];
        if (x == RARE) {
            f[32] = 1.5f;
            d[63] = -0.0;
            return Float.floatToIntBits(f[32]) + Double.doubleToRawLongBits(d[63]) + f.length + d.length
                    + Arrays.hashCode(f) + Arrays.hashCode(d);
        }
        return x + f.length + d.length;
    }

    static long e4(int x) {
        int[] small = new int[8];
        int[] edge = new int[9];
        int[] over = new int[65];
        if (x == RARE) {
            small[7] = 1;
            edge[8] = 2;
            over[64] = 3;
            return small.length + edge.length * 100L + over.length * 10_000L + small[7] + edge[8] + over[64]
                    + Arrays.stream(over).sum();
        }
        return x;
    }

    static long e5(int x) {
        int[] a = new int[20];
        int[] b = new int[20];
        int[] pick = (x & 1) == 0 ? a : b;
        if (x == RARE || x == RARE + 1) {
            pick[3] = 11;
            return a[3] * 10L + b[3] + (pick == a ? 1000 : 0) + (pick == b ? 2000 : 0);
        }
        return x + pick.length;
    }

    // The same, in a loop that runs long enough to OSR, with the rare trip
    // somewhere in the middle.
    static long osr(int n, int rareAt) {
        long acc = 0;
        for (int k = 0; k < n; k++) {
            long[] t = new long[48];
            acc += t.length;
            if (k == rareAt) {
                t[k % 48] = k;
                acc += t[k % 48] * 3 + t[(k + 1) % 48] + (t.getClass() == long[].class ? 5 : 0);
            }
        }
        return acc;
    }

    static String row(int x) {
        return e0(x) + " " + e1(x) + " " + e2(x) + " " + e3(x) + " " + e4(x) + " " + e5(x);
    }

    public static void main(String[] args) {
        int[] xs = {0, 1, -1, 1000};
        for (int x : xs) {
            System.out.println("row cold x=" + x + " | " + row(x));
        }
        System.out.println("rare cold | " + row(RARE) + " | " + e5(RARE + 1));
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + e0(r) + e1(r) + e2(r) + e3(r) + e4(r) + e5(r);
        }
        System.out.println("warm hash " + h);
        for (int x : xs) {
            System.out.println("row hot x=" + x + " | " + row(x));
        }
        // The deopt trigger: each rare branch, once.
        System.out.println("rare hot | " + row(RARE) + " | " + e5(RARE + 1));
        System.out.println("osr " + osr(2_000_000, -1) + " " + osr(2_000_000, 1_500_000) + " " + osr(300, 299));
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + e0(r) + e1(r) + e2(r) + e3(r) + e4(r) + e5(r);
        }
        System.out.println("rewarm hash " + h);
        System.out.println("rare again | " + row(RARE) + " | " + e5(RARE + 1));
        for (int x : xs) {
            System.out.println("row after x=" + x + " | " + row(x));
        }
    }
}
