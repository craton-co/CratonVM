// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

/**
 * gcj j1/og (2026-10-19): a tenured live set between this collector's
 * construction split of the old generation ({@code -Xmx / 2}) and HotSpot
 * Serial's old maximum ({@code 2/3 · -Xmx}), with steady young churn and a
 * slow promotion trickle -- the shape of
 * {@code gengc-jr20-javac-footprint-wide-references} (javac at 256m). For
 * {@code CRATONVM_GC_OLD_FREE_RATIO_GROWTH}.
 *
 * <p>HotSpot Serial expands its tenured generation after a full collection
 * until {@code MinHeapFreeRatio} (40 %) is free, so the live set runs at about
 * 60 % occupancy and full collections are rare. Without the switch CratonVM's
 * old generation stays at its construction capacity until an allocation is
 * REFUSED, so the live set sits above the 75 % occupancy trigger and a major
 * runs on almost every young cycle.
 *
 * <p>Fixed work: {@code steps} steps, each allocating {@code churnMb} MiB of
 * short-lived garbage and replacing {@code replace} of the live chunks (the
 * trickle: the replaced chunk dies in the old generation, the fresh one is
 * promoted). stdout is identical on HotSpot and CratonVM:
 * <pre>
 *   Gcj1OgFreeRatioGrowthProbe liveMb=96 steps=400 churnMb=8 replace=4
 *   chunks=1536 checksum=&lt;n&gt;
 *   PROBE-OK
 * </pre>
 * stderr (informational): {@code [probe] loop-ms=<wall> full-gc-count=<n>
 * young-gc-count=<n>}. The deciding numbers are the {@code [GC]} summary's
 * {@code trig_collections} (old collections) and, with the j1/og print hunk,
 * {@code oldsz_free_ratio_growths} under {@code CRATONVM_GC_STATS=1}.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/probes Gcj1OgFreeRatioGrowthProbe
 *   CRATONVM_GC_STATS=1 CRATONVM_GC_OLD_FREE_RATIO_GROWTH=1 cratonvm -XX:+UseGenerationalGC \
 *       -Xmx256m -cp tools/probes Gcj1OgFreeRatioGrowthProbe
 * </pre>
 * Args (all optional): liveMb steps churnMb replace.
 */
public final class Gcj1OgFreeRatioGrowthProbe {
    /** One live chunk: 64 KiB of payload spread over 64 small reference-holding nodes. */
    static final class Chunk {
        final int id;
        final Object[] cells = new Object[64];

        Chunk(int id) {
            this.id = id;
            for (int i = 0; i < cells.length; i++) {
                cells[i] = new long[127];
            }
        }

        long sum() {
            long s = id;
            for (Object c : cells) {
                s += ((long[]) c).length;
            }
            return s;
        }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        int liveMb = args.length > 0 ? Integer.parseInt(args[0]) : 96;
        int steps = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int churnMb = args.length > 2 ? Integer.parseInt(args[2]) : 8;
        int replace = args.length > 3 ? Integer.parseInt(args[3]) : 4;
        System.out.println("Gcj1OgFreeRatioGrowthProbe liveMb=" + liveMb + " steps=" + steps
                + " churnMb=" + churnMb + " replace=" + replace);
        int nChunks = liveMb * 16; // 64 KiB each
        Chunk[] live = new Chunk[nChunks];
        for (int i = 0; i < nChunks; i++) {
            live[i] = new Chunk(i);
        }
        long t0 = System.nanoTime();
        int cursor = 0;
        for (int s = 0; s < steps; s++) {
            for (int k = 0; k < churnMb * 16; k++) {
                sink = new byte[64 * 1024 - 64];
            }
            for (int r = 0; r < replace; r++) {
                live[cursor] = new Chunk(live[cursor].id);
                cursor = (cursor + 1) % nChunks;
            }
        }
        long loopMs = (System.nanoTime() - t0) / 1_000_000;
        long checksum = 0;
        for (Chunk c : live) {
            checksum += c.sum();
        }
        System.out.println("chunks=" + nChunks + " checksum=" + checksum);
        System.out.println("PROBE-OK");
        long full = 0;
        long young = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            String name = b.getName();
            long c = Math.max(0, b.getCollectionCount());
            if (name.contains("MarkSweep") || name.contains("Old") || name.contains("Full")) {
                full += c;
            } else {
                young += c;
            }
        }
        System.err.println("[probe] loop-ms=" + loopMs + " full-gc-count=" + full
                + " young-gc-count=" + young);
    }
}
