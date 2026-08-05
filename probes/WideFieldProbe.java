// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * COV-03 — {@code long} / {@code float} / {@code double} instance fields
 * through the optimizing (C2/IR) tier.
 *
 * <p>A wide field reads through the same checked {@code jit_getfield} helper an
 * int or reference field does, and the helper returns the long payload or the
 * FP BIT PATTERN in RAX. That creates a collision the int and reference cases
 * do not have: the helper's deopt/NPE sentinel is {@code i64::MIN}, and
 * {@code Long.MIN_VALUE} — and the bits of {@code -0.0d} — are the same
 * sixty-four bits. The tier disambiguates with the out-of-band
 * {@code jit_dispatch_threw} peek, exactly as it does for a {@code J}/{@code D}
 * call return.
 *
 * <p>So the values below are not a random sample. {@code Long.MIN_VALUE} and
 * {@code -0.0d} are the two that fail if the peek is wrong in the direction of
 * "treat it as a throw" (the read bails or throws a phantom NPE); every other
 * value is a control that fails if the read is broken generally. NaN is here
 * because a bit-pattern round trip and a numeric round trip are different
 * claims, and {@code doubleToRawLongBits} is what tells them apart.
 *
 * <pre>
 *   CRATONVM_JIT=force-c2 CRATONVM_DBG=ir-compiles \
 *     cratonvm -cp probes WideFieldProbe
 * </pre>
 */
public class WideFieldProbe {

    static final class Box {
        long l;
        float f;
        double d;
        int guardBefore;
        int guardAfter;
    }

    // The methods under test: one bare field access each.
    static void setL(Box b, long v) {
        b.l = v;
    }

    static long getL(Box b) {
        return b.l;
    }

    static void setF(Box b, float v) {
        b.f = v;
    }

    static float getF(Box b) {
        return b.f;
    }

    static void setD(Box b, double v) {
        b.d = v;
    }

    static double getD(Box b) {
        return b.d;
    }

    private static final long[] LONGS = {
        0L, 1L, -1L, 42L, Long.MAX_VALUE,
        Long.MIN_VALUE, // bit-identical to the helper's deopt sentinel
        Long.MIN_VALUE + 1, 0x8000_0000_0000_0001L, 0x0000_0000_8000_0000L,
    };

    private static final float[] FLOATS = {
        0.0f, -0.0f, 1.0f, -1.0f, 3.14159f, Float.MIN_VALUE, Float.MAX_VALUE,
        Float.NaN, Float.POSITIVE_INFINITY, Float.NEGATIVE_INFINITY,
    };

    private static final double[] DOUBLES = {
        0.0d,
        -0.0d, // raw bits == Long.MIN_VALUE == the deopt sentinel
        1.0d, -1.0d, 3.141592653589793d, Double.MIN_VALUE, Double.MAX_VALUE,
        Double.NaN, Double.POSITIVE_INFINITY, Double.NEGATIVE_INFINITY,
    };

    public static void main(String[] args) {
        final int warm = Integer.getInteger("probe.warm", 300000);
        Box b = new Box();
        b.guardBefore = 0x5A5A5A5A;
        b.guardAfter = 0x0F0F0F0F;

        int failures = 0;

        // Warm every accessor past the C2 threshold on ordinary values, so the
        // interesting ones below are read and written by a COMPILED body rather
        // than by the interpreter.
        for (int i = 0; i < warm; i++) {
            setL(b, i);
            setF(b, i);
            setD(b, i);
            if (getL(b) != i || getF(b) != (float) i || getD(b) != (double) i) {
                System.out.println("[widefield] FAIL warm-up round-trip at i=" + i);
                failures++;
                break;
            }
        }

        for (long v : LONGS) {
            setL(b, v);
            long got = getL(b);
            if (got != v) {
                System.out.println("[widefield] FAIL long " + v + " -> " + got);
                failures++;
            }
        }

        for (float v : FLOATS) {
            setF(b, v);
            float got = getF(b);
            if (Float.floatToRawIntBits(got) != Float.floatToRawIntBits(v)) {
                System.out.println("[widefield] FAIL float bits "
                        + Integer.toHexString(Float.floatToRawIntBits(v)) + " -> "
                        + Integer.toHexString(Float.floatToRawIntBits(got)));
                failures++;
            }
        }

        for (double v : DOUBLES) {
            setD(b, v);
            double got = getD(b);
            if (Double.doubleToRawLongBits(got) != Double.doubleToRawLongBits(v)) {
                System.out.println("[widefield] FAIL double bits "
                        + Long.toHexString(Double.doubleToRawLongBits(v)) + " -> "
                        + Long.toHexString(Double.doubleToRawLongBits(got)));
                failures++;
            }
        }

        // A wide store writes a wider cell than an int store does. If it wrote
        // through the wrong lowering — or at the wrong packed offset — the
        // neighbouring slots are what it lands in.
        if (b.guardBefore != 0x5A5A5A5A || b.guardAfter != 0x0F0F0F0F) {
            System.out.println("[widefield] FAIL neighbouring field clobbered: before="
                    + Integer.toHexString(b.guardBefore)
                    + " after=" + Integer.toHexString(b.guardAfter));
            failures++;
        }

        // A null receiver must still throw, on both arms. The helper returns
        // silently on an implausible receiver, so an unguarded call would turn
        // a NullPointerException into a dropped store / fabricated read.
        failures += expectNpe("getL", () -> getL(null));
        failures += expectNpe("getF", () -> getF(null));
        failures += expectNpe("getD", () -> getD(null));
        failures += expectNpe("setL", () -> setL(null, 1L));
        failures += expectNpe("setF", () -> setF(null, 1.0f));
        failures += expectNpe("setD", () -> setD(null, 1.0d));

        System.out.println("[widefield] failures=" + failures);
        System.out.println(failures == 0 ? "[widefield] PASS" : "[widefield] FAIL");
        if (failures != 0) {
            System.exit(1);
        }
    }

    private static int expectNpe(String what, Runnable r) {
        try {
            r.run();
        } catch (NullPointerException expected) {
            return 0;
        } catch (Throwable t) {
            System.out.println("[widefield] FAIL " + what + " on null threw " + t);
            return 1;
        }
        System.out.println("[widefield] FAIL " + what + " on null did not throw");
        return 1;
    }
}
