// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;

/**
 * gcd d4/k (2026-09-28): the per-call cost number the default flip of
 * {@code CRATONVM_JNI_INDIRECT_LOCALS} + {@code CRATONVM_JNI_NATIVE_TRANSITIONS}
 * waits for ({@code docs/known-issues/gc/gcd-d2i-jni-native-methods-are-counted-mutators-20260927.md},
 * flip gate item 4).
 *
 * <p>Four cases, each timed as a median over {@link #REPS} repetitions after a
 * warm-up, reported in ns per call on stderr only:
 * <ul>
 *   <li>{@code noop}: a Java loop calling an empty native (the dispatch, and
 *       with the flags on the in-native bracket);</li>
 *   <li>{@code array-length}: one native looping {@code GetArrayLength};</li>
 *   <li>{@code int-region}: one native looping a one-element
 *       {@code GetIntArrayRegion};</li>
 *   <li>{@code new-string}: one native looping {@code NewStringUTF} +
 *       {@code GetStringUTFLength} + {@code DeleteLocalRef}.</li>
 * </ul>
 *
 * <p>stdout is deterministic: one {@code <case>: ok} line per case (its
 * checksum matched) and {@code PASS all 4}, which HotSpot prints too; the
 * numbers are on stderr as {@code [gcd1-jni-cost] <case> median=<ns>ns/call
 * reps=[...]}. Compare arms by the medians, interleaving binaries/arms run by
 * run (in-process timings on this host swing about 3x between runs).
 *
 * <p>Commands (Linux):
 * <pre>
 *   gcc -O1 -shared -fPIC -I"$JDK/include" -I"$JDK/include/linux" \
 *       -o /tmp/libgcd1jnicost.so tools/probes/jni/Gcd1JniCostProbe.c
 *   javac -d /tmp/gcd1jnicost tools/bench/Gcd1JniCostProbe.java
 *   java -XX:+UseSerialGC -Xmx256m -cp /tmp/gcd1jnicost Gcd1JniCostProbe /tmp/libgcd1jnicost.so
 * </pre>
 */
public final class Gcd1JniCostProbe {

    static native int noop(int x);

    static native long loopArrayLength(int[] a, int n);

    static native long loopRegion(int[] a, int n);

    static native long loopNewString(int n);

    static final int REPS = 7;
    static final int CALLS = 200_000;

    interface Case {
        /** Runs {@code n} calls; returns the checksum. */
        long run(int n);
    }

    static int failures;

    static void measure(String name, Case c, long expected) {
        c.run(CALLS / 10); // warm-up (tiers the Java loop up); checksum unchecked
        long[] ns = new long[REPS];
        boolean ok = true;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            long sum = c.run(CALLS);
            ns[r] = (System.nanoTime() - t0) / CALLS;
            ok &= sum == expected;
        }
        long[] sorted = ns.clone();
        Arrays.sort(sorted);
        System.err.println("[gcd1-jni-cost] " + name + " median=" + sorted[REPS / 2]
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
            System.out.println("usage: Gcd1JniCostProbe /abs/path/libgcd1jnicost.so");
            System.exit(2);
        }
        System.load(args[0]);
        int[] a = new int[64];
        long regionSum = 0;
        for (int i = 0; i < a.length; i++) {
            a[i] = i;
        }
        for (int i = 0; i < CALLS; i++) {
            regionSum += i % a.length;
        }
        final long region = regionSum;

        measure("noop", n -> {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += noop(i);
            }
            return s;
        }, (long) CALLS * (CALLS + 1) / 2);
        measure("array-length", n -> loopArrayLength(a, n), (long) CALLS * a.length);
        measure("int-region", n -> loopRegion(a, n), region);
        measure("new-string", n -> loopNewString(n), 4L * CALLS);

        if (failures == 0) {
            System.out.println("PASS all 4");
        } else {
            System.out.println("FAIL " + failures + " of 4");
            System.exit(1);
        }
    }
}
