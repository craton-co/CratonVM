// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w4/oldgen4 (2026-09-24): after the live set drops, the old generation
 * must give its memory back, so {@code Runtime.totalMemory()} falls.
 *
 * <p>Retains 160 MiB of 1 MiB {@code byte[]}s, collects so they are old, and
 * samples {@code totalMemory} right after a {@code System.gc()} ({@code peak});
 * then drops them and calls {@code System.gc()} six times (HotSpot shrinks in
 * steps: 0 %, 10 %, 40 %, 100 % of the excess over consecutive full GCs; CratonVM
 * damps the first shrinking collection after growth) and samples again
 * ({@code after}), also right after a {@code System.gc()}, so the young
 * generation is in the same state at both samples and the difference is the old
 * generation's. {@code shrank=true} means it fell by at least half the dropped
 * bytes.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xms8m -Xmx512m -cp tools/bench GenR4W4ShrinkProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xms8m -Xmx512m -cp tools/bench GenR4W4ShrinkProbe
 * </pre>
 * HotSpot prints, and CratonVM must print:
 * <pre>
 *   shrank=true
 *   PASS
 * </pre>
 * Negative control: {@code CRATONVM_GC_OLD_SHRINK=0} → {@code shrank=false},
 * {@code FAIL} on CratonVM. Add {@code -v} to print the figures.
 */
public final class GenR4W4ShrinkProbe {
    static final int MIB = 1 << 20;
    static final int COUNT = 160;
    static List<byte[]> live = new ArrayList<>();

    static void fill() {
        for (int i = 0; i < COUNT; i++) {
            byte[] a = new byte[MIB];
            a[0] = (byte) i;
            a[MIB - 1] = (byte) ~i;
            live.add(a);
        }
    }

    public static void main(String[] args) {
        boolean verbose = args.length > 0 && args[0].equals("-v");
        Runtime rt = Runtime.getRuntime();
        fill();
        for (int i = 0; i < 3; i++) {
            System.gc();
        }
        long peak = rt.totalMemory();
        boolean intact = true;
        for (int i = 0; i < COUNT; i++) {
            byte[] a = live.get(i);
            intact &= a[0] == (byte) i && a[MIB - 1] == (byte) ~i;
        }
        live = null;
        for (int i = 0; i < 6; i++) {
            System.gc();
        }
        long after = rt.totalMemory();
        long dropped = (long) COUNT * MIB;
        boolean shrank = peak - after >= dropped / 2;
        if (verbose) {
            System.out.println("peak=" + peak + " after=" + after + " dropped=" + dropped);
        }
        if (!intact) {
            System.out.println("retained FAILED: an array was corrupted before the drop");
        }
        System.out.println("shrank=" + shrank);
        boolean ok = shrank && intact;
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
