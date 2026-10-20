// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i5 wave 105, lane L2. A spliced `a / b` under
// `if (b != 0)` next to a spliced `a / b` under `b == 0` (which must throw).
// `decide_branch_implied_guards` used to decide the first division's
// zero-divisor guard from the `b != 0` edge; the division is a floating node
// whose zero test the lowerer keys off that guard, so it lowered to a bare
// IDIV that ran with b == 0 (EXCEPTION_INT_DIVIDE_BY_ZERO / #DE, a VM crash;
// docs/internal/fixed-bugs/jit-ncw-branch-decided-guards-sigfpe-in-a-freed-code-buffer-FIXED-20261016.md).
// The answer is folded into a hash that is computed cold and then hot.
//
// Run (JIT on, both modes, a few times each; must never crash):
//   javac -d out L2W105SplicedDivUnderZeroTest.java
//   cratonvm -cp out L2W105SplicedDivUnderZeroTest
//
// HOTSPOT_EXPECTED_BEGIN
// int same
// long same
// rem same
// HOTSPOT_EXPECTED_END
public class L2W105SplicedDivUnderZeroTest {
    static final int[] IV = {0, 1, -1, 7, -7, 2, 0, Integer.MIN_VALUE, Integer.MAX_VALUE, 0, 3};
    static final long[] LV = {0L, 1L, -1L, 7L, Long.MIN_VALUE, 0L, Long.MAX_VALUE, 5L};

    static int idiv(int a, int b) { return a / b; }
    static int irem(int a, int b) { return a % b; }
    static long ldiv(long a, long b) { return a / b; }

    static long mix(long h, long v) { return h * 31 + v; }

    static long ints() {
        long h = 17;
        for (int a : IV) for (int b : IV) {
            if (b == 0) {
                try { h = mix(h, idiv(a, b)); } catch (ArithmeticException e) { h = mix(h, 0x5A5A); }
                continue;
            }
            h = mix(h, idiv(a, b));
        }
        return h;
    }

    static long rems() {
        long h = 17;
        for (int a : IV) for (int b : IV) {
            if (b != 0) {
                h = mix(h, irem(a, b));
            } else {
                try { h = mix(h, irem(a, b)); } catch (ArithmeticException e) { h = mix(h, 0x5B5B); }
            }
        }
        return h;
    }

    static long longs() {
        long h = 17;
        for (long a : LV) for (long b : LV) {
            if (b == 0) {
                try { h = mix(h, ldiv(a, b)); } catch (ArithmeticException e) { h = mix(h, 0x5C5C); }
                continue;
            }
            h = mix(h, ldiv(a, b));
        }
        return h;
    }

    public static void main(String[] args) {
        long ci = ints(), cl = longs(), cr = rems();
        long hi = 0, hl = 0, hr = 0;
        boolean si = true, sl = true, sr = true;
        for (int r = 0; r < 30000; r++) {
            hi = ints();
            hl = longs();
            hr = rems();
            si &= hi == ci;
            sl &= hl == cl;
            sr &= hr == cr;
        }
        System.out.println("int " + (si ? "same" : "DIFF " + Long.toHexString(ci) + " " + Long.toHexString(hi)));
        System.out.println("long " + (sl ? "same" : "DIFF " + Long.toHexString(cl) + " " + Long.toHexString(hl)));
        System.out.println("rem " + (sr ? "same" : "DIFF " + Long.toHexString(cr) + " " + Long.toHexString(hr)));
    }
}
