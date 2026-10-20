// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j2/og2 (2026-10-19): the old generation's committed size after a live
 * set is dropped, over consecutive full collections. For
 * {@code CRATONVM_GC_OLD_SHRINK_FACTOR} (O1 of
 * {@code gcj-j1og-concurrent-cycle-and-old-gen-review-residuals}).
 *
 * <p>HotSpot Serial damps its tenured shrink with {@code _shrink_factor}: the
 * first full collection that finds the generation too large releases 0 % of
 * the excess over {@code MaxHeapFreeRatio}, the next 10 %, then 40 %, then
 * 100 %. CratonVM's default releases nothing on the first and everything on
 * the second; with the switch it steps as HotSpot does.
 *
 * <p>Fixed work: build {@code liveMb} MiB of tenured live data (promoted by
 * {@code System.gc()}), drop it, then run {@code gcs} more
 * {@code System.gc()} calls. stdout is identical on HotSpot and CratonVM:
 * <pre>
 *   Gcj2Og2ShrinkStepsProbe liveMb=96 gcs=6
 *   checksum=&lt;n&gt;
 *   PROBE-OK
 * </pre>
 * stderr (informational, compare with HotSpot's): one
 * {@code [probe] gc=<i> totalMemory-mb=<n>} line per collection after the
 * drop. The deciding numbers are that sequence and, under
 * {@code CRATONVM_GC_STATS=1}, the {@code [GC] oldgen_sizing:} line's
 * {@code oldsz_shrinks} / {@code oldsz_shrunk_bytes} / {@code oldsz_shrinks_damped}.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx512m -cp tools/probes Gcj2Og2ShrinkStepsProbe
 *   CRATONVM_GC_STATS=1 CRATONVM_GC_OLD_SHRINK_FACTOR=1 cratonvm -XX:+UseGenerationalGC \
 *       -Xmx512m -cp tools/probes Gcj2Og2ShrinkStepsProbe
 * </pre>
 * Args (all optional): liveMb gcs.
 */
public final class Gcj2Og2ShrinkStepsProbe {
    static volatile Object sink;

    public static void main(String[] args) {
        int liveMb = args.length > 0 ? Integer.parseInt(args[0]) : 96;
        int gcs = args.length > 1 ? Integer.parseInt(args[1]) : 6;
        System.out.println("Gcj2Og2ShrinkStepsProbe liveMb=" + liveMb + " gcs=" + gcs);
        // 64 KiB per chunk: 16 long[511] arrays (4 KiB each).
        int nChunks = liveMb * 16;
        Object[][] live = new Object[nChunks][];
        long checksum = 0;
        for (int i = 0; i < nChunks; i++) {
            Object[] chunk = new Object[16];
            for (int j = 0; j < chunk.length; j++) {
                long[] a = new long[511];
                a[0] = i;
                a[510] = j;
                chunk[j] = a;
            }
            live[i] = chunk;
        }
        // Promote the live set.
        System.gc();
        System.gc();
        for (Object[] chunk : live) {
            for (Object o : chunk) {
                long[] a = (long[]) o;
                checksum += a[0] + a[510];
            }
        }
        // Drop it, and watch the committed size over the next collections.
        live = null;
        sink = null;
        Runtime rt = Runtime.getRuntime();
        for (int g = 1; g <= gcs; g++) {
            System.gc();
            System.err.println("[probe] gc=" + g + " totalMemory-mb=" + (rt.totalMemory() >> 20));
        }
        System.out.println("checksum=" + checksum);
        System.out.println("PROBE-OK");
    }
}
