// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L7: what the long and widening forms of
// the fused array accumulate (CRATONVM_INTERP_FUSED_ARRAY_ACCUMULATE_LONG,
// default off) must leave unchanged: the sums (wrapping, every long
// arithmetic the group takes, every widened element type, sign / zero
// extension), the store into another local, and the exceptions an element
// read raises at its own pc with the accumulator untouched. Run with the
// switch off and on; both must print the block below.
//
// Run: cratonvm --java-home <jdk25> --nojit -cp <dir> L7W82LongAccumulate
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// add 9223372036854775806
// sub -9223372036854775807
// mul 0
// and 0
// or 9223372036854775807
// xor 9223372036854775804
// widenInt -4294967296
// widenByte -384
// widenChar 196605
// widenShort -98304
// otherLocal 6 t=9
// aioobe Index 3 out of bounds for length 3 s=3
// npe java.lang.NullPointerException s=7
// HOTSPOT_EXPECTED_END
public class L7W82LongAccumulate {
    // Locals in every method: arr 0, s 1..2, i 3 (the fused group's
    // placement).
    static long add(long[] la) {
        long s = Long.MAX_VALUE;
        for (int i = 0; i < la.length; i++) {
            s += la[i];
        }
        return s;
    }

    static long sub(long[] la) {
        long s = Long.MIN_VALUE;
        for (int i = 0; i < la.length; i++) {
            s -= la[i];
        }
        return s;
    }

    static long mul(long[] la) {
        long s = 1L << 62;
        for (int i = 0; i < la.length; i++) {
            s *= la[i];
        }
        return s;
    }

    static long and(long[] la) {
        long s = -1L;
        for (int i = 0; i < la.length; i++) {
            s &= la[i];
        }
        return s;
    }

    static long or(long[] la) {
        long s = Long.MAX_VALUE - 7;
        for (int i = 0; i < la.length; i++) {
            s |= la[i];
        }
        return s;
    }

    static long xor(long[] la) {
        long s = Long.MAX_VALUE;
        for (int i = 0; i < la.length; i++) {
            s ^= la[i];
        }
        return s;
    }

    static long widenInt(int[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static long widenByte(byte[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static long widenChar(char[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static long widenShort(short[] a) {
        long s = 0;
        for (int i = 0; i < a.length; i++) {
            s += a[i];
        }
        return s;
    }

    static long last;

    // Locals: la 0, s 1..2, i 3, t 4..5: `t = s + la[i]` stores into local
    // 4 (`lstore 4`, not the fused group's `lstore_N`), so only the plain
    // path runs it; the sum `s` still fuses.
    static String otherLocal(long[] la) {
        long s = 0;
        for (int i = 0; i < la.length; i++) {
            s += la[i];
            long t = s + la[i];
            last = t;
        }
        return s + " t=" + last;
    }

    static long s;

    // Locals: la 0, acc 1..2, i 3; reads one element past the end.
    static void overrun(long[] la) {
        long acc = 0;
        for (int i = 0; i <= la.length; i++) {
            acc += la[i];
            s = acc;
        }
    }

    // Locals: la 0, acc 1..2, i 3; the array is null.
    static void nullArray(long[] la) {
        long acc = 7;
        s = acc;
        for (int i = 0; i < 2; i++) {
            acc += la[i];
            s = acc;
        }
    }

    public static void main(String[] args) {
        System.out.println("add " + add(new long[] {-1}));
        System.out.println("sub " + sub(new long[] {-1}));
        System.out.println("mul " + mul(new long[] {4}));
        System.out.println("and " + and(new long[] {Long.MIN_VALUE, Long.MAX_VALUE}));
        System.out.println("or " + or(new long[] {1, 2, 4}));
        System.out.println("xor " + xor(new long[] {1, 2}));
        System.out.println("widenInt " + widenInt(new int[] {Integer.MIN_VALUE, Integer.MIN_VALUE}));
        System.out.println("widenByte " + widenByte(new byte[] {-128, -128, -128}));
        System.out.println("widenChar " + widenChar(new char[] {'￿', '￿', '￿'}));
        System.out.println("widenShort " + widenShort(new short[] {Short.MIN_VALUE, Short.MIN_VALUE, Short.MIN_VALUE}));
        System.out.println("otherLocal " + otherLocal(new long[] {1, 2, 3}));
        s = 0;
        try {
            overrun(new long[] {1, 1, 1});
        } catch (ArrayIndexOutOfBoundsException e) {
            System.out.println("aioobe " + e.getMessage() + " s=" + s);
        }
        s = 0;
        try {
            nullArray(null);
        } catch (NullPointerException e) {
            System.out.println("npe " + e.getClass().getName() + " s=" + s);
        }
    }
}
