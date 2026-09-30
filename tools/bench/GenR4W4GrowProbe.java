// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w4/oldgen4 (2026-09-24): the old generation's committed size must grow
 * with a live set that outgrows the initial heap, and a live set below the old
 * generation's share of {@code -Xmx} must never end in
 * {@code OutOfMemoryError}.
 *
 * <p>Retains 256 KiB {@code byte[]} chunks (below every humongous threshold, so
 * they are promoted through the young generation like ordinary survivors) until
 * {@code pct} percent of {@code Runtime.maxMemory()} is live, then checks every
 * chunk's contents. {@code grow ok} means {@code Runtime.totalMemory()} rose by at
 * least half the retained bytes over its value at start. No numbers are printed
 * on the default path, so the output is comparable byte for byte.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xms8m -Xmx256m -cp tools/bench GenR4W4GrowProbe 35
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xms8m -Xmx256m -cp tools/bench GenR4W4GrowProbe 35
 * </pre>
 * HotSpot prints, and CratonVM must print:
 * <pre>
 *   grow ok
 *   retained ok
 *   PASS
 * </pre>
 * With {@code 60} instead of {@code 35}: HotSpot (old generation = 2/3 of the heap
 * under its default {@code NewRatio=2}) still passes; CratonVM's default split
 * gives the old generation 1/2 of {@code -Xmx}, so it prints
 * {@code grow FAILED: OutOfMemoryError} and {@code FAIL} — the gap filed as
 * {@code docs/internal/gaps/gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget-20260924.md}.
 * With {@code -XX:NewRatio=2 ... 60} both pass. Add {@code -v} as a second
 * argument to print the {@code totalMemory} figures.
 */
public final class GenR4W4GrowProbe {
    static final int CHUNK = 256 * 1024;

    public static void main(String[] args) {
        int pct = args.length > 0 ? Integer.parseInt(args[0]) : 35;
        boolean verbose = args.length > 1 && args[1].equals("-v");
        Runtime rt = Runtime.getRuntime();
        long max = rt.maxMemory();
        long target = max / 100 * pct;
        long start = rt.totalMemory();
        List<byte[]> live = new ArrayList<>();
        long retained = 0;
        boolean ok = true;
        try {
            while (retained < target) {
                byte[] c = new byte[CHUNK];
                int i = live.size();
                c[0] = (byte) i;
                c[CHUNK - 1] = (byte) (i * 31 + 7);
                live.add(c);
                retained += CHUNK;
                // Some short-lived garbage between survivors, so young
                // collections run and promote the chunks as they would in an
                // application.
                if ((i & 7) == 0) {
                    byte[] g = new byte[CHUNK];
                    g[1] = 1;
                }
            }
            long end = rt.totalMemory();
            if (verbose) {
                System.out.println("max=" + max + " start=" + start + " end=" + end + " retained=" + retained);
            }
            if (end - start >= retained / 2) {
                System.out.println("grow ok");
            } else {
                ok = false;
                System.out.println("grow FAILED: totalMemory rose by less than half the live set");
            }
        } catch (OutOfMemoryError e) {
            live = null;
            System.out.println("grow FAILED: OutOfMemoryError");
            System.out.println("FAIL");
            System.exit(1);
            return;
        }
        boolean intact = true;
        for (int i = 0; i < live.size(); i++) {
            byte[] c = live.get(i);
            if (c[0] != (byte) i || c[CHUNK - 1] != (byte) (i * 31 + 7)) {
                intact = false;
                break;
            }
        }
        System.out.println(intact ? "retained ok" : "retained FAILED: a chunk was corrupted");
        ok &= intact;
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
