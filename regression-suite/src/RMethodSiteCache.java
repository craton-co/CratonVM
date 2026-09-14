// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;
import java.util.concurrent.CountDownLatch;

/**
 * Regression: the interpreter's per-thread resolved-METHOD site cache
 * (`CRATONVM_JIT=method-site-cache`) must never answer a call site with another
 * site's descriptor or parameter count.
 *
 * `pop_coerced_invoke_args_virtual` / `_static` use exactly two things out of a
 * resolved method reference — the descriptor and the parameter slot count — to
 * decide how many operands to pop and how to decode each one. The site cache
 * supplies both without re-resolving. Both failure modes are silent and neither
 * throws:
 *
 *   * a wrong PARAMETER COUNT pops too few or too many operands, so the callee
 *     receives shifted arguments and the caller's operand stack is left
 *     misaligned;
 *   * a wrong DESCRIPTOR gives `nth_param_tag_byte` the wrong tag per slot, so
 *     a `long` is decoded as an `int` (or two ints), a `float`'s bit pattern is
 *     read as an integer, and a reference slot is read as a primitive.
 *
 * So the vectors below are built around the cases where those two mistakes
 * produce a DIFFERENT answer rather than a crash:
 *
 *   1. overload sets     — `Math.abs`/`max`/`min` exist as (I)I, (J)J, (F)F and
 *                          (D)D. Same owner, same name, four descriptors, four
 *                          constant-pool indices. Values are chosen so that
 *                          answering one with another's descriptor changes the
 *                          result (e.g. -3.5f vs -3L vs -3).
 *   2. category-2 layout — arguments where a long or double sits BEFORE another
 *                          argument, so mis-sizing it shifts everything after
 *                          it. `Math.copySign`, `Long.compare`,
 *                          `String.indexOf(int,int)`, `Arrays.fill(long[],int,int,long)`.
 *   3. bit-exact codecs  — `Double.doubleToRawLongBits` / `Float.floatToRawIntBits`
 *                          and their inverses round-trip values whose bit
 *                          patterns are NOT their numeric value, so a decode
 *                          through the wrong tag is visible.
 *   4. mixed ref/prim    — `System.arraycopy(Object,int,Object,int,int)`, whose
 *                          five slots alternate reference and primitive.
 *   5. slot conflicts    — hundreds of distinct call sites cycled so the
 *                          direct-mapped table evicts continuously.
 *   6. per-thread        — the table lives on `JvmThread`; four threads must
 *                          each build their own and agree.
 *
 * ⚠️ In the default CORE run the flag is OFF, so this exercises the ORDINARY
 * `resolve_method_ref` path — a real check, but NOT the cached one it was
 * written for. To cover that path it must be run explicitly:
 *
 *     CRATONVM_JIT=method-site-cache ONLY=RMethodSiteCache bash regression-suite/run.sh
 *
 * Fold it into the default set only when that flag's default flips.
 */
public class RMethodSiteCache {

    static int checks = 0;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError(m);
        }
    }

    static void checkEq(long actual, long expected, String m) {
        checks++;
        if (actual != expected) {
            throw new AssertionError(m + ": got " + actual + ", expected " + expected);
        }
    }

    static void checkBits(double actual, double expected, String m) {
        checks++;
        if (Double.doubleToRawLongBits(actual) != Double.doubleToRawLongBits(expected)) {
            throw new AssertionError(m + ": got " + actual + ", expected " + expected);
        }
    }

    static void checkBits(float actual, float expected, String m) {
        checks++;
        if (Float.floatToRawIntBits(actual) != Float.floatToRawIntBits(expected)) {
            throw new AssertionError(m + ": got " + actual + ", expected " + expected);
        }
    }

    public static void main(String[] args) throws Exception {
        overloadSets();
        categoryTwoLayout();
        bitExactCodecs();
        mixedRefAndPrimitive();
        slotConflicts();
        perThreadIsolation();

        System.out.println("CK RMethodSiteCache checks=" + checks);
        System.out.println("PASS RMethodSiteCache");
    }

    // ---------------------------------------------------------------- 1 ----
    static void overloadSets() {
        // Distinct magnitudes per type: if `abs(J)J`'s site were answered with
        // `abs(I)I`'s descriptor the long argument would be popped as one slot
        // and the result would not be 9000000000L.
        int i = -1234567;
        long j = -9_000_000_000L;
        float f = -3.5f;
        double d = -2.718281828459045;

        for (int n = 0; n < 60_000; n++) {
            if (Math.abs(i) != 1234567) {
                throw new AssertionError("Math.abs(int) at n=" + n + ": " + Math.abs(i));
            }
            if (Math.abs(j) != 9_000_000_000L) {
                throw new AssertionError("Math.abs(long) at n=" + n + ": " + Math.abs(j));
            }
            if (Math.abs(f) != 3.5f) {
                throw new AssertionError("Math.abs(float) at n=" + n + ": " + Math.abs(f));
            }
            if (Math.abs(d) != 2.718281828459045) {
                throw new AssertionError("Math.abs(double) at n=" + n + ": " + Math.abs(d));
            }
            // max/min across the same four descriptors, interleaved so the
            // sites keep displacing each other in a direct-mapped table.
            if (Math.max(i, 0) != 0 || Math.min(i, 0) != i) {
                throw new AssertionError("Math.max/min(int) at n=" + n);
            }
            if (Math.max(j, 0L) != 0L || Math.min(j, 0L) != j) {
                throw new AssertionError("Math.max/min(long) at n=" + n);
            }
            if (Math.max(f, 0f) != 0f || Math.min(f, 0f) != f) {
                throw new AssertionError("Math.max/min(float) at n=" + n);
            }
            if (Math.max(d, 0d) != 0d || Math.min(d, 0d) != d) {
                throw new AssertionError("Math.max/min(double) at n=" + n);
            }
        }
        checkEq(Math.abs(i), 1234567, "Math.abs(int)");
        checkEq(Math.abs(j), 9_000_000_000L, "Math.abs(long)");
        checkBits(Math.abs(f), 3.5f, "Math.abs(float)");
        checkBits(Math.abs(d), 2.718281828459045, "Math.abs(double)");

        // String.valueOf's overload set spans reference and every primitive
        // width, so a descriptor mix-up changes the rendered text.
        check("-9000000000".equals(String.valueOf(j)), "String.valueOf(long)");
        check("-1234567".equals(String.valueOf(i)), "String.valueOf(int)");
        check("-3.5".equals(String.valueOf(f)), "String.valueOf(float)");
        check("true".equals(String.valueOf(true)), "String.valueOf(boolean)");
        check("z".equals(String.valueOf('z')), "String.valueOf(char)");
        check("null".equals(String.valueOf((Object) null)), "String.valueOf(Object)");
    }

    // ---------------------------------------------------------------- 2 ----
    static void categoryTwoLayout() {
        // A category-2 argument sitting BEFORE another argument: mis-sizing it
        // shifts every slot after it.
        for (int n = 0; n < 60_000; n++) {
            // copySign(double magnitude, double sign) — two cat-2 slots.
            if (Math.copySign(3.25, -1.0) != -3.25) {
                throw new AssertionError("Math.copySign at n=" + n);
            }
            // compare(long, long) — two cat-2 slots, an int result.
            if (Long.compare(-9_000_000_000L, 9_000_000_000L) >= 0) {
                throw new AssertionError("Long.compare at n=" + n);
            }
            // scalb(double, int) — cat-2 THEN cat-1: the classic shift case.
            if (Math.scalb(1.5, 4) != 24.0) {
                throw new AssertionError("Math.scalb at n=" + n);
            }
            // (int, int) after a receiver — the virtual, non-static shape.
            if ("abcabc".indexOf('b', 2) != 4) {
                throw new AssertionError("String.indexOf(int,int) at n=" + n);
            }
        }
        checkBits(Math.copySign(3.25, -1.0), -3.25, "Math.copySign");
        checkEq(Long.compare(-9_000_000_000L, 9_000_000_000L), -1, "Long.compare");
        checkBits(Math.scalb(1.5, 4), 24.0, "Math.scalb");
        checkEq("abcabc".indexOf('b', 2), 4, "String.indexOf(int,int)");

        // Arrays.fill(long[], int from, int to, long value): a cat-1 pair
        // between the reference and the cat-2 value.
        long[] la = new long[8];
        Arrays.fill(la, 2, 6, 0x0102030405060708L);
        checkEq(la[0], 0L, "Arrays.fill left edge");
        checkEq(la[1], 0L, "Arrays.fill before from");
        checkEq(la[2], 0x0102030405060708L, "Arrays.fill at from");
        checkEq(la[5], 0x0102030405060708L, "Arrays.fill at to-1");
        checkEq(la[6], 0L, "Arrays.fill at to");
        checkEq(la[7], 0L, "Arrays.fill right edge");

        // Same shape on doubles, so the cat-2 value is a double not a long.
        double[] da = new double[8];
        Arrays.fill(da, 2, 6, 2.5);
        checkBits(da[1], 0.0, "Arrays.fill(double) before from");
        checkBits(da[2], 2.5, "Arrays.fill(double) at from");
        checkBits(da[5], 2.5, "Arrays.fill(double) at to-1");
        checkBits(da[6], 0.0, "Arrays.fill(double) at to");
    }

    // ---------------------------------------------------------------- 3 ----
    static void bitExactCodecs() {
        // Bit patterns that are not their numeric value, so a decode through
        // the wrong descriptor tag is visible rather than merely imprecise.
        long lbits = 0x400921FB54442D18L; // pi
        double pi = Double.longBitsToDouble(lbits);
        int fbits = 0x40490FDB; // pi as a float
        float pif = Float.intBitsToFloat(fbits);

        long acc = 0;
        for (int n = 0; n < 60_000; n++) {
            acc ^= Double.doubleToRawLongBits(pi);
            acc ^= Float.floatToRawIntBits(pif);
            acc ^= Double.doubleToLongBits(-0.0);
            acc ^= Long.reverse(0x0123456789ABCDEFL);
        }
        checkEq(Double.doubleToRawLongBits(pi), lbits, "doubleToRawLongBits round trip");
        checkEq(Float.floatToRawIntBits(pif), fbits, "floatToRawIntBits round trip");
        checkEq(Double.doubleToLongBits(-0.0), 0x8000000000000000L, "doubleToLongBits(-0.0)");
        checkEq(Long.reverse(0x0123456789ABCDEFL), 0xF7B3D591E6A2C480L, "Long.reverse");
        checkEq(Long.numberOfTrailingZeros(0x0000000100000000L), 32,
                "Long.numberOfTrailingZeros");
        checkEq(Long.bitCount(0xFFFFFFFFFFFFFFFFL), 64, "Long.bitCount");
        checkEq(Integer.bitCount(0xFFFFFFFF), 32, "Integer.bitCount");
        // The accumulator exists so the loop is not dead code; an even
        // iteration count cancels every xor.
        checkEq(acc, 0L, "even iteration count must cancel the xor accumulator");
    }

    // ---------------------------------------------------------------- 4 ----
    static void mixedRefAndPrimitive() {
        // arraycopy(Object,int,Object,int,int): reference, prim, reference,
        // prim, prim. A shifted pop puts an int where a reference belongs.
        int[] src = new int[16];
        for (int k = 0; k < 16; k++) {
            src[k] = 1000 + k;
        }
        for (int n = 0; n < 20_000; n++) {
            int[] dst = new int[16];
            System.arraycopy(src, 3, dst, 5, 7);
            if (dst[4] != 0 || dst[5] != 1003 || dst[11] != 1009 || dst[12] != 0) {
                throw new AssertionError("System.arraycopy at n=" + n + ": " + Arrays.toString(dst));
            }
        }
        int[] dst = new int[16];
        System.arraycopy(src, 3, dst, 5, 7);
        checkEq(dst[4], 0, "arraycopy before dstPos");
        checkEq(dst[5], 1003, "arraycopy at dstPos");
        checkEq(dst[11], 1009, "arraycopy at dstPos+len-1");
        checkEq(dst[12], 0, "arraycopy at dstPos+len");

        // Object-typed arrays, so the copied slots really are references.
        String[] ssrc = {"a", "b", "c", "d", "e"};
        String[] sdst = new String[5];
        System.arraycopy(ssrc, 1, sdst, 2, 3);
        check(sdst[0] == null && sdst[1] == null, "arraycopy(String[]) head");
        check("b".equals(sdst[2]) && "c".equals(sdst[3]) && "d".equals(sdst[4]),
                "arraycopy(String[]) body: " + Arrays.toString(sdst));
    }

    // ---------------------------------------------------------------- 5 ----
    static void slotConflicts() {
        // Many distinct call sites cycled together, each with a signature whose
        // slot layout differs from its neighbours', so a table entry serving
        // the wrong site produces a wrong VALUE rather than a crash.
        long total = 0;
        for (int round = 0; round < 4_000; round++) {
            total += Math.abs(-round);
            total += Math.abs(-(long) round * 1_000_000_000L) / 1_000_000_000L;
            total += (long) Math.abs(-(float) round);
            total += (long) Math.abs(-(double) round);
            total += Long.compare(round, 0) + 1;
            total += Integer.compare(round, 0) + 1;
            total += Long.bitCount(round);
            total += Integer.bitCount(round);
            total += Math.max(round, 0);
            total += Math.max((long) round, 0L);
            total += (long) Math.max((float) round, 0f);
            total += (long) Math.max((double) round, 0d);
            total += "abcabc".indexOf('c', 1);
            total += Long.numberOfTrailingZeros(1L << (round % 63));
            total += String.valueOf(round).length();
            total += Math.floorMod(round, 7);
        }
        long expected = 0;
        for (int round = 0; round < 4_000; round++) {
            expected += round;                       // abs(int)
            expected += round;                       // abs(long)
            expected += round;                       // abs(float)
            expected += round;                       // abs(double)
            expected += (round > 0 ? 1 : 0) + 1;     // Long.compare
            expected += (round > 0 ? 1 : 0) + 1;     // Integer.compare
            expected += Long.bitCount(round);
            expected += Integer.bitCount(round);
            expected += round;                       // max(int)
            expected += round;                       // max(long)
            expected += round;                       // max(float)
            expected += round;                       // max(double)
            expected += 2;                           // "abcabc".indexOf('c', 1)
            expected += round % 63;                  // trailing zeros
            expected += String.valueOf(round).length();
            expected += round % 7;
        }
        checkEq(total, expected, "rotating method-site calls");
    }

    // ---------------------------------------------------------------- 6 ----
    static void perThreadIsolation() throws Exception {
        final int threads = 4;
        final CountDownLatch start = new CountDownLatch(1);
        final Throwable[] failure = new Throwable[1];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final long seed = (t + 1) * 1_000_000_007L;
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                    for (int n = 0; n < 20_000; n++) {
                        if (Math.abs(-seed) != seed) {
                            throw new AssertionError("abs(long) in worker");
                        }
                        if (Math.abs(-(int) (seed % 1000)) != (int) (seed % 1000)) {
                            throw new AssertionError("abs(int) in worker");
                        }
                        if (Double.doubleToRawLongBits(Double.longBitsToDouble(seed)) != seed) {
                            throw new AssertionError("double bit round trip in worker");
                        }
                        if (Long.compare(seed, 0L) != 1) {
                            throw new AssertionError("Long.compare in worker");
                        }
                    }
                } catch (Throwable ex) {
                    synchronized (failure) {
                        if (failure[0] == null) {
                            failure[0] = ex;
                        }
                    }
                }
            });
            ts[t].start();
        }
        start.countDown();
        for (Thread t : ts) {
            t.join(120_000);
            check(!t.isAlive(), "worker thread did not finish");
        }
        synchronized (failure) {
            if (failure[0] != null) {
                throw new AssertionError("worker failure: " + failure[0], failure[0]);
            }
        }
        checks++;
    }
}
