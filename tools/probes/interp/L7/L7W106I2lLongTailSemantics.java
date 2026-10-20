// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 106, lane L7: what CRATONVM_INTERP_FUSED_I2L_LONG_TAIL (default
// off) must leave unchanged. Every `i2l; <long op> [; lstore]` form javac emits for a long
// left operand and an int expression: operand order (lsub), sign extension of a negative or
// overflowed int, land / lor / lxor / lmul wrap-around, `lstore n` past local 3, the push
// form (no store) with values below on the stack, lcmp at the long extremes, and `i2l;
// lstore` (not a tail). Run each line under `(no env)` and `CRATONVM_INTERP_FUSED_I2L_LONG_TAIL=1`,
// with and without `--nojit`, both modes: all SAME.
// HOTSPOT_EXPECTED_BEGIN
// subOrder -2440
// negExt 42423808
// andMask 1152921504606846471
// orIn 8064
// mulExpr 2422921986435413697
// wideStore 34300
// pushForm 2453
// cmpExtremes 50 0 50 0
// widen -26
// HOTSPOT_EXPECTED_END
public class L7W106I2lLongTailSemantics {
    static final class Holder {
        int f;
    }

    static long subOrder(int n) {
        long s = 10;
        for (int i = 0; i < n; i++) {
            s -= i * 2;
        }
        return s;
    }

    static long negExt(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += (i - 5) * 1_000_000_000; // int overflow, then sign extension
        }
        return s;
    }

    static long andMask(int n) {
        long s = Long.MAX_VALUE >>> 3;
        for (int i = 0; i < n; i++) {
            s &= ~(i << 3);
        }
        return s;
    }

    static long orIn(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s |= i << 7;
        }
        return s;
    }

    static long mulExpr(int n) {
        long s = 1;
        for (int i = 0; i < n; i++) {
            s *= i | 0x7654321;
        }
        return s;
    }

    // `s` is in locals 5-6: `lstore 5`.
    static long wideStore(long a, long b, int n) {
        long s = a + b;
        for (int i = 0; i < n; i++) {
            s += i * 28;
        }
        return s;
    }

    static long consume(int k, long v) {
        return k * 1000L + v;
    }

    // `consume(7, s + h.f)`: an int below the long and the int on the stack.
    static long pushForm(Holder h, int n) {
        long s = 0;
        long acc = 0;
        for (int i = 0; i < n; i++) {
            h.f = i;
            acc += consume(7, s + h.f);
            s += 1;
        }
        return acc - 7 * 1000L * n + (s ^ h.f);
    }

    static String cmpExtremes(int n) {
        long min = Long.MIN_VALUE;
        long max = Long.MAX_VALUE;
        int a = 0;
        int b = 0;
        int c = 0;
        int d = 0;
        for (int i = 0; i < n; i++) {
            if (min < i - 3) {
                a++;
            }
            if (max < i - 3) {
                b++;
            }
            if (max > Integer.MIN_VALUE + i) {
                c++;
            }
            if (min > Integer.MIN_VALUE + i) {
                d++;
            }
        }
        return a + " " + b + " " + c + " " + d;
    }

    static long widen(int n) {
        long last = 0;
        for (int i = 0; i < n; i++) {
            long x = i - 75;
            last = x;
        }
        return last;
    }

    public static void main(String[] args) {
        int n = 50;
        System.out.println("subOrder " + subOrder(n));
        System.out.println("negExt " + negExt(n));
        System.out.println("andMask " + andMask(n));
        System.out.println("orIn " + orIn(n));
        System.out.println("mulExpr " + mulExpr(n));
        System.out.println("wideStore " + wideStore(0, 0, n));
        System.out.println("pushForm " + pushForm(new Holder(), n));
        System.out.println("cmpExtremes " + cmpExtremes(n));
        System.out.println("widen " + widen(n));
    }
}
