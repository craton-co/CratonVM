// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;

/**
 * gci i16/dp (2026-10-14; i15/jk note R1, {@code gcd-d5f-jni-default-dispatch-costs-500ns}): the
 * {@code va_list} up-call forms. Every {@code Call<Type>Method(...)} (variadic, through the VM's
 * trampoline) and {@code Call<Type>MethodV} reads its arguments through {@code va_list_to_jvalues}, which
 * built a {@code Vec<JValue>} (plus a descriptor-cache probe and a {@code Vec} copy) per call; since i16 it
 * reads the tags in place and reuses one per-thread buffer of 16. This probe checks the values (checksums
 * HotSpot prints too) and times each shape.
 *
 * <p>Cases, each a native loop of {@code n} up-calls from C:
 * <ul>
 *   <li>{@code add3}: {@code CallStaticIntMethod(cls, add3, i, 1, 2)} (variadic, three ints);</li>
 *   <li>{@code v-form}: the same through an explicit {@code CallStaticIntMethodV};</li>
 *   <li>{@code mixed}: {@code CallStaticLongMethod(cls, mix, int, long, double, float, Object)} (a promoted
 *       float, both register classes);</li>
 *   <li>{@code nested}: a variadic up-call to {@code outer}, whose Java body calls the native {@code inner},
 *       which makes another variadic up-call while the outer call's argument buffer is still out;</li>
 *   <li>{@code wide18}: eighteen int arguments (past the per-thread buffer: an array of its own).</li>
 * </ul>
 *
 * <p>stdout is deterministic: one {@code <case>: ok} line per case and {@code PASS all 5}. Timings go to
 * stderr: {@code [gcidp16-va] <case> median=<ns>ns/call reps=[...]}. Compare binaries by medians,
 * interleaved run by run.
 *
 * <p>Build: {@code javac --release 25 GciDp16JniVaCallProbe.java}; the native half is
 * {@code tools/probes/jni/GciDp16JniVaCallProbe.c}. Run: {@code GciDp16JniVaCallProbe /abs/path/lib}.
 */
public final class GciDp16JniVaCallProbe {

    static native long loopAdd3(int n);

    static native long loopV(int n);

    static native long loopMixed(int n, Object o);

    static native long loopNested(int n);

    static native long loopWide(int n);

    /** Called by {@code outer}: one variadic up-call to {@code add3(i, 10, 20)}. */
    static native int inner(int i);

    static int add3(int a, int b, int c) {
        return a + b + c;
    }

    static long mix(int a, long b, double d, float f, Object o) {
        return a + b + (long) (d * 2) + (long) (f * 2) + (o == null ? 0 : 7);
    }

    static int outer(int i) {
        return inner(i) + 1;
    }

    static int wide18(int a0, int a1, int a2, int a3, int a4, int a5, int a6, int a7, int a8,
            int a9, int a10, int a11, int a12, int a13, int a14, int a15, int a16, int a17) {
        return a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8 + a9 + a10 + a11 + a12 + a13 + a14 + a15
                + a16 + a17;
    }

    static final int REPS = 7;
    static final int CALLS = 100_000;

    interface Case {
        /** Runs {@code n} up-calls; returns the checksum. */
        long run(int n);
    }

    static int failures;

    /** sum of i for i in [0, n). */
    static long tri(long n) {
        return n * (n - 1) / 2;
    }

    static void measure(String name, Case c, long perCallConstant, long perIndex) {
        int warm = CALLS / 10;
        long warmSum = c.run(warm);
        boolean ok = warmSum == perIndex * tri(warm) + perCallConstant * warm;
        long[] ns = new long[REPS];
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long sum = c.run(CALLS);
            ns[r] = (System.nanoTime() - t0) / CALLS;
            ok &= sum == perIndex * tri(CALLS) + perCallConstant * CALLS;
        }
        long[] sorted = ns.clone();
        Arrays.sort(sorted);
        System.err.println("[gcidp16-va] " + name + " median=" + sorted[REPS / 2]
                + "ns/call reps=" + Arrays.toString(ns));
        if (ok) {
            System.out.println(name + ": ok");
        } else {
            failures++;
            System.out.println(name + ": FAIL checksum");
        }
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: GciDp16JniVaCallProbe /abs/path/libgcidp16");
            System.exit(2);
        }
        System.load(args[0]);
        Object token = new Object();
        measure("add3", GciDp16JniVaCallProbe::loopAdd3, 3, 1);
        measure("v-form", GciDp16JniVaCallProbe::loopV, 9, 1);
        // mix(i, 3i, 0.5, 1.5f, token) = 4i + 1 + 3 + 7
        measure("mixed", n -> loopMixed(n, token), 11, 4);
        // outer(i) = inner(i) + 1 = (i + 10 + 20) + 1
        measure("nested", GciDp16JniVaCallProbe::loopNested, 31, 1);
        // wide18(i, 1, 2, ..., 17) = i + 153
        measure("wide18", GciDp16JniVaCallProbe::loopWide, 153, 1);
        if (failures == 0) {
            System.out.println("PASS all 5");
        } else {
            System.out.println("FAIL " + failures + " of 5");
            System.exit(1);
        }
    }
}
