// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gcd d10/o retirement (2026-10-05): does a collector give back what a
 * program dropped after it filled the heap? Fills the heap with 1 KiB arrays
 * through one of three holders until {@code OutOfMemoryError}, drops the
 * holder, runs {@code System.gc()} three times and prints the heap's used
 * kilobytes after each.
 * {@code docs/known-issues/gc/gcd-d10o-g1-keeps-dropped-data-after-a-heap-filling-oome-20261005.md}.
 *
 * <pre>
 *   java     -XX:+UseG1GC -Xmx64m -cp out Gcd10oG1DropRetentionProbe list|array|chain
 *   cratonvm -XX:+UseG1GC -Xmx64m -cp out Gcd10oG1DropRetentionProbe list|array|chain
 * </pre>
 *
 * <p>HotSpot 25.0.4 G1 prints about 1.3 MB for every holder; CratonVM's ZGC
 * and Generational backends print under 1 MB (Generational after its second
 * collection).
 */
public final class Gcd10oG1DropRetentionProbe {
    static List<long[]> list;
    static Object[] array;
    static Object[] chain;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "list";
        Runtime rt = Runtime.getRuntime();
        try {
            switch (mode) {
                case "list":
                    list = new ArrayList<>();
                    while (true) {
                        list.add(new long[128]);
                    }
                case "array":
                    array = new Object[200_000];
                    for (int i = 0; ; i++) {
                        array[i] = new long[128];
                    }
                default:
                    while (true) {
                        Object[] n = new Object[2];
                        n[0] = chain;
                        n[1] = new long[128];
                        chain = n;
                    }
            }
        } catch (OutOfMemoryError | ArrayIndexOutOfBoundsException e) {
            list = null;
            array = null;
            chain = null;
        }
        StringBuilder sb = new StringBuilder(mode).append(" usedKB after System.gc():");
        for (int g = 0; g < 3; g++) {
            System.gc();
            sb.append(' ').append((rt.totalMemory() - rt.freeMemory()) / 1024);
        }
        System.out.println(sb);
    }
}
