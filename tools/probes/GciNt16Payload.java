// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.function.IntUnaryOperator;

/**
 * gci i16/nt -- the class every {@code GciNt16NonHeapThresholdProbe.OneShot}
 * loader defines from this file's bytes: some methods and constants, so each
 * definition adds class metadata, and a hot {@code applyAsInt} the JIT
 * compiles once per defined class. Public (the probe instantiates each
 * definition reflectively from another runtime package); references only
 * {@code java.base}. Not a probe on its own (no {@code main}).
 */
public final class GciNt16Payload implements IntUnaryOperator {
    static final String A = "gci-nt16-payload-constant-a";
    static final String B = "gci-nt16-payload-constant-b";
    int f0, f1, f2, f3, f4, f5, f6, f7;

    public GciNt16Payload() {}

    @Override
    public int applyAsInt(int x) {
        int y = x * 31 + f0;
        if ((y & 1) == 0) {
            y ^= (y >>> 7) + f1;
        } else {
            y += (y << 3) - f2;
        }
        for (int i = 0; i < 4; i++) {
            y = y * 17 + i + f3;
        }
        return y + m0(x) + m1(y);
    }

    int m0(int x) { return x * 3 + f4 + A.length(); }
    int m1(int x) { return x ^ (x >>> 3) + f5 + B.length(); }
    int m2(int x) { return (x << 2) - f6; }
    int m3(int x) { return x % 7 + f7; }
    int m4(int x) { return m2(x) + m3(x); }
    int m5(int x) { return m4(x) * 5 - 1; }
    int m6(int x) { return m5(x) ^ 0x5a5a; }
    int m7(int x) { return m6(x) + "m7".hashCode(); }
    int m8(int x) { return m7(x) - "m8".hashCode(); }
    int m9(int x) { return m8(x) * "m9".length(); }
    int m10(int x) { return Integer.rotateLeft(x, 3) + "m10".hashCode(); }
    int m11(int x) { return Integer.rotateRight(x, 5) - "m11".hashCode(); }
    int m12(int x) { return Long.hashCode(x * 1_000_003L) + "m12".length(); }
    int m13(int x) { return Math.abs(x) + "m13".hashCode(); }
    int m14(int x) { return Math.max(x, 13) - "m14".hashCode(); }
    int m15(int x) { return Math.min(x, 15) ^ "m15".hashCode(); }
    long l0(long v) { return v * 0x9E3779B97F4A7C15L; }
    long l1(long v) { return v ^ (v >>> 33); }
    long l2(long v) { return l0(l1(v)); }
    double d0(double v) { return Math.sqrt(Math.abs(v)) + 0.5; }
    double d1(double v) { return d0(v) * d0(v + 1); }
}
