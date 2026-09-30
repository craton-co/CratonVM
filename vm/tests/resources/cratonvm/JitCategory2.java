// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
package cratonvm;

// Regression fixture for the JIT early-compile category-2 (long/double)
// PARAMETER bug: the early-compile path laid params out sequentially by
// argument index (param_slots = args.len()), so the 2nd long/double param
// was read from the wrong (empty) JVM local slot. `secondLong(100,23)`
// returned 0 and `addLongs(100,23)` returned 100 instead of 123.
public class JitCategory2 {
    static long addLongs(long a, long b) { return a + b; }
    static long secondLong(long a, long b) { return b; }
    static double addDoubles(double a, double b) { return a + b; }
    static long threeLongs(long a, long b, long c) { return c; }

    // Cold (first-call) variants — exercise eager early-compile.
    static long addOnce() { return addLongs(100L, 23L); }       // 123
    static long secondOnce() { return secondLong(100L, 23L); }  // 23
    static long thirdOnce() { return threeLongs(1L, 2L, 3L); }  // 3

    // Hot drivers — force JIT compilation via a counted loop, so a
    // miscompiled body shifts the aggregate detectably.
    static long driveAdd() {
        long t = 0;
        for (int i = 0; i < 500; i++) t += addLongs(100L, 23L);
        return t; // 500 * 123 = 61500
    }
    static long driveSecond() {
        long t = 0;
        for (int i = 0; i < 500; i++) t += secondLong(100L, 23L);
        return t; // 500 * 23 = 11500
    }
    static long driveDoubleBits() {
        double t = 0;
        for (int i = 0; i < 500; i++) t += addDoubles(1.5, 2.25);
        return Double.doubleToRawLongBits(t); // bits of 1875.0
    }
}
