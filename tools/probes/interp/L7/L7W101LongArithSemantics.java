// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 101, lane L7: the answers of the long groups
// CRATONVM_INTERP_FUSED_LONG_ARITH fuses (`lload; {lload | lconst}; ladd |
// lsub | lmul | land | lor | lxor [; lstore]` and `...; lcmp`), over
// wrap-around values and long bit patterns that collide with the compact
// value's NaN tags (0xFFFC..., 0x7FF8..., 0xFFF8...). The output must be
// identical with the switch off, with it on, and on HotSpot.
//
// Run:
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L7W101LongArithSemantics
//   CRATONVM_INTERP_FUSED_LONG_ARITH=1 cratonvm --java-home <jdk25> --nojit -cp <dir> L7W101LongArithSemantics
// (and both again without --nojit).
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// fold=2317262
// same 0: 0 0 0 0 1 0 0 false false true
// next 0: 1 ffffffffffffffff 0 1 2 ffffffffffffffff 0 true true false
// same 1: 2 0 1 1 3 1 1 false true true
// next 1: 0 2 ffffffffffffffff ffffffffffffffff 1 fffffffffffffffd 1 false false true
// same 2: fffffffffffffffe 0 1 ffffffffffffffff ffffffffffffffff 1 ffffffffffffffff false false false
// next 2: 7fffffffffffffff 7fffffffffffffff 8000000000000000 ffffffffffffffff 8000000000000000 ffffffffffffffff ffffffffffffffff false false false
// same 3: 0 0 0 8000000000000000 1 0 8000000000000000 false false false
// next 3: ffffffffffffffff 1 8000000000000000 ffffffffffffffff 0 8000000000000001 8000000000000000 true false false
// same 4: fffffffffffffffe 0 1 7fffffffffffffff ffffffffffffffff 1 7fffffffffffffff false false false
// next 4: 7ffc000012345677 8003ffffedcba987 3ffffedcba988 ffffffffffffffff 7ffc000012345678 800000000000000f 7fffffffffffffff false true false
// same 5: fff800002468acf0 0 4d8b66dc1df4d840 fffc000012345678 fff800002468acf1 4d8b66dc1df4d840 fffc000012345678 false false false
// next 5: 7ff4000012345679 8004000012345677 4c3c000012345678 fffc000012345679 7ff400001234567a cc3800000000000f fffc000012345678 true true false
// same 6: fff0000000000002 0 fff0000000000001 7ff8000000000001 fff0000000000003 fff0000000000001 7ff8000000000001 false false false
// next 6: 7ff0000000000001 8000000000000001 fff8000000000000 fff8000000000001 7ff0000000000002 7ff8000000000001 7ff8000000000001 false true false
// same 7: fff0000000000000 0 0 fff8000000000000 fff0000000000001 0 fff8000000000000 false false false
// next 7: fff87048860ddf79 fff78fb779f22087 438000000000000 fff87048860ddf79 fff87048860ddf7a fbcf8fb779f22087 fff8000000000000 true true false
// same 8: e0910c1bbef2 0 d18a1ac3215e0731 7048860ddf79 e0910c1bbef3 d18a1ac3215e0731 7048860ddf79 false true false
// next 8: 70484b2f76c8 7048c0ec482a 4d182bc78b15557 ffffffffc72ddf7f 70484b2f76c9 4d1f2f4b85d1d7d 7048860ddf79 false false false
// same 9: ffffffff8a432e9e 0 d8988a9f1cc4a61 ffffffffc521974f ffffffff8a432e9f d8988a9f1cc4a61 ffffffffc521974f false false false
// next 9: ffffffffc521974f ffffffffc521974f 0 ffffffffc521974f ffffffffc5219750 ffffffffc521974f ffffffffc521974f true false false
// HOTSPOT_EXPECTED_END
public class L7W101LongArithSemantics {
    static final long[] VALUES = {
        0L,
        1L,
        -1L,
        Long.MIN_VALUE,
        Long.MAX_VALUE,
        0xFFFC_0000_1234_5678L,
        0x7FF8_0000_0000_0001L,
        0xFFF8_0000_0000_0000L,
        123_456_789_012_345L,
        -987_654_321L,
    };

    // a in locals 0-1, b in 2-3: `a + b` is lload_0; lload_2; ladd; lstore 4,
    // `x + 1` is lload 4; lconst_1; ladd; lstore 12, `a - 0L` is lload_0;
    // lconst_0; lsub; lstore 16, and each comparison is lload; lload; lcmp.
    static String row(long a, long b) {
        long x = a + b;
        long y = a - b;
        long z = a * b;
        long w = (a & b) | (a ^ b);
        long p = x + 1;
        long q = y ^ z;
        long r = a - 0L;
        boolean lt = a < b;
        boolean gt = x > y;
        boolean eq = z == w;
        return Long.toHexString(x) + " " + Long.toHexString(y) + " " + Long.toHexString(z)
                + " " + Long.toHexString(w) + " " + Long.toHexString(p) + " "
                + Long.toHexString(q) + " " + Long.toHexString(r) + " " + lt + " " + gt
                + " " + eq;
    }

    public static void main(String[] args) {
        // Warm the method so a JIT run also compiles it; the answers must not move.
        long fold = 0;
        for (int k = 0; k < 20_000; k++) {
            fold += row(VALUES[k % VALUES.length], VALUES[(k / 3) % VALUES.length]).length();
        }
        System.out.println("fold=" + fold);
        for (int i = 0; i < VALUES.length; i++) {
            long a = VALUES[i];
            System.out.println("same " + i + ": " + row(a, a));
            System.out.println("next " + i + ": " + row(a, VALUES[(i + 1) % VALUES.length]));
        }
    }
}
