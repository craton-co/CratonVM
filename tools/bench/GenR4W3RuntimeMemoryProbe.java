// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w3/hunter (2026-09-23): {@code Runtime.totalMemory/freeMemory/maxMemory}
 * must obey HotSpot's invariants on the generational backend, before and after
 * young collections, after a large retained allocation, and after it is dropped.
 *
 * <p>Invariants checked at every sample (all HotSpot collectors satisfy them):
 * <ol>
 *   <li>{@code 0 <= free <= total <= max};
 *   <li>{@code max} is constant for the life of the process and is within
 *       [{@code Xmx}/2, {@code Xmx}] (HotSpot's Serial/Parallel subtract one
 *       survivor space; G1 reports {@code Xmx} exactly);
 *   <li>retaining 48 MiB raises {@code used = total - free} by at least 40 MiB
 *       (slack for concurrent JDK-internal churn);
 *   <li>dropping it and calling {@code System.gc()} lowers {@code used} by at
 *       least 40 MiB again.
 * </ol>
 * Deterministic output on HotSpot (each line {@code ok}):
 * <pre>
 *   bounds ok
 *   max-constant ok
 *   max-vs-xmx ok
 *   used-grows ok
 *   used-shrinks ok
 *   PASS
 * </pre>
 * Commands (the argument is the -Xmx in MiB):
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4W3RuntimeMemoryProbe 256
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W3RuntimeMemoryProbe 256
 * </pre>
 * A failing line prints the offending numbers.
 */
public final class GenR4W3RuntimeMemoryProbe {
    static final long MiB = 1024L * 1024L;
    static boolean ok = true;
    static String boundsWhy = null;

    static volatile Object sink;

    static long[] sample(Runtime rt) {
        final long max = rt.maxMemory();
        final long total = rt.totalMemory();
        final long free = rt.freeMemory();
        if (boundsWhy == null && !(0 <= free && free <= total && total <= max)) {
            boundsWhy = "free=" + free + " total=" + total + " max=" + max;
        }
        return new long[] {max, total, free, total - free};
    }

    static void line(String name, boolean pass, String detail) {
        System.out.println(name + (pass ? " ok" : " FAILED " + detail));
        ok &= pass;
    }

    public static void main(String[] args) {
        final long xmxMiB = args.length > 0 ? Long.parseLong(args[0]) : 256;
        final Runtime rt = Runtime.getRuntime();
        final long max0 = rt.maxMemory();
        boolean maxConstant = true;

        // Churn: several young collections, sampling between them.
        for (int i = 0; i < 64; i++) {
            for (int j = 0; j < 1024; j++) {
                sink = new byte[1024];
            }
            maxConstant &= sample(rt)[0] == max0;
        }
        sink = null;
        System.gc();
        final long[] before = sample(rt);

        final List<byte[]> keep = new ArrayList<>();
        for (int i = 0; i < 48; i++) {
            keep.add(new byte[(int) MiB]);
            maxConstant &= sample(rt)[0] == max0;
        }
        final long[] grown = sample(rt);
        keep.clear();
        System.gc();
        final long[] shrunk = sample(rt);
        maxConstant &= shrunk[0] == max0;

        line("bounds", boundsWhy == null, String.valueOf(boundsWhy));
        line("max-constant", maxConstant, "max0=" + max0 + " now=" + rt.maxMemory());
        line("max-vs-xmx", max0 <= xmxMiB * MiB && max0 >= xmxMiB * MiB / 2,
                "max=" + max0 + " xmx=" + xmxMiB * MiB);
        line("used-grows", grown[3] - before[3] >= 40 * MiB,
                "before=" + before[3] + " after=" + grown[3]);
        line("used-shrinks", grown[3] - shrunk[3] >= 40 * MiB,
                "retained=" + grown[3] + " dropped=" + shrunk[3]);
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
