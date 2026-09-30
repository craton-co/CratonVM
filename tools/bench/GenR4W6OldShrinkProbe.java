// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;
import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w6/oldpin6 (2026-09-24): the old generation gives its memory back after
 * the live set drops, WITHOUT {@code System.gc()}. The sibling
 * {@code GenR4W4ShrinkProbe} calls {@code System.gc()}, a requested and hence
 * stop-the-world collection, so it cannot see that under the concurrent-first
 * policy a program that peaks and settles may never get a stop-the-world
 * old-gen collection at all
 * ({@code docs/internal/gaps/gengc-r4w5-oldcompact5-old-gen-shrink-waits-for-a-stop-the-world-collection-20260924.md}).
 *
 * <p>Grows the old generation to 192 MiB of retained 256 KiB {@code byte[]}s
 * (allocated with young garbage in between, then 1 GiB more of garbage so they
 * are tenured), samples the heap's committed size through
 * {@code MemoryMXBean} ({@code peak}), drops all but the first 16 chunks IN
 * PLACE (no new list is allocated, so nothing new is tenured above the kept
 * chunks), then only allocates young garbage (3 GiB in 64 KiB arrays) and
 * samples again ({@code after}). {@code PASS shrunk} means the committed heap
 * fell by at least half of the dropped bytes. The old pool's committed size
 * ({@code MemoryPoolMXBean}, "Old"/"Tenured") is printed with the figures.
 *
 * <p>Deterministic output: the checksum (the garbage bytes allocated plus the
 * kept chunks' first and last bytes) and the verdict. The figures are printed
 * only with {@code -v}, on lines that start with {@code info (not deterministic)}.
 *
 * <pre>
 *   java -XX:+UseG1GC -Xms8m -Xmx384m -cp tools/bench GenR4W6OldShrinkProbe
 *   CRATONVM_GC_OLD_SHRINK_AFTER_CONCURRENT=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xms8m -Xmx384m -XX:NewRatio=2 -cp tools/bench GenR4W6OldShrinkProbe
 * </pre>
 * Expected (HotSpot G1 shrinks at the remark of the concurrent cycle that
 * finds the dropped chunks dead; CratonVM with the flag at the young pause
 * after that cycle's sweep):
 * <pre>
 *   checksum=4697628160
 *   PASS shrunk
 * </pre>
 * Negative controls: CratonVM without the flag prints the same checksum and
 * {@code FAIL} (no stop-the-world old-gen collection runs after the drop, so
 * nothing resizes); HotSpot {@code -XX:+UseSerialGC} prints {@code FAIL} too,
 * because Serial resizes its tenured generation only after a full collection.
 * {@code -XX:NewRatio=2} gives CratonVM HotSpot's two-thirds old generation.
 */
public final class GenR4W6OldShrinkProbe {
    static final int KIB = 1024;
    static final int MIB = 1 << 20;
    static final int CHUNK = 256 * KIB;
    /** 192 MiB retained. */
    static final int RETAIN = 768;
    /** 4 MiB survive the drop. */
    static final int KEEP = 16;
    static final int GARBAGE = 64 * KIB;

    /** Sized once, up front, so the list itself is tenured early and low. */
    static final List<byte[]> live = new ArrayList<>(RETAIN);
    static volatile Object sink;
    static long churned;

    /** Allocate {@code bytes} of garbage in 64 KiB arrays. */
    static void churn(long bytes) {
        long n = bytes / GARBAGE;
        for (long i = 0; i < n; i++) {
            byte[] g = new byte[GARBAGE];
            g[0] = (byte) i;
            sink = g;
            churned += g.length;
        }
        sink = null;
    }

    static long heapCommitted() {
        return ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getCommitted();
    }

    static long oldPoolCommitted() {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            String n = p.getName();
            if (p.getType() == MemoryType.HEAP && (n.contains("Old") || n.contains("Tenured"))) {
                return p.getUsage().getCommitted();
            }
        }
        return -1;
    }

    public static void main(String[] args) {
        boolean verbose = args.length > 0 && args[0].equals("-v");
        // Create the platform beans before the growth, so they are tenured
        // below the chunks rather than above them.
        long start = heapCommitted();
        long startOld = oldPoolCommitted();

        long peak = start;
        for (int i = 0; i < RETAIN; i++) {
            byte[] a = new byte[CHUNK];
            a[0] = (byte) i;
            a[CHUNK - 1] = (byte) ~i;
            live.add(a);
            if ((i & 15) == 15) {
                churn(8L * MIB);
                peak = Math.max(peak, heapCommitted());
            }
        }
        churn(1024L * MIB);
        peak = Math.max(peak, heapCommitted());
        long peakOld = oldPoolCommitted();

        boolean intact = live.size() == RETAIN;
        for (int i = 0; i < RETAIN && intact; i++) {
            byte[] a = live.get(i);
            intact = a[0] == (byte) i && a[CHUNK - 1] == (byte) ~i;
        }

        // The drop, in place: `removeRange` shifts nothing (it is the tail)
        // and nulls the slots; no new object is allocated.
        live.subList(KEEP, RETAIN).clear();

        long after = Long.MAX_VALUE;
        for (int round = 0; round < 48; round++) {
            churn(64L * MIB);
            after = heapCommitted();
        }
        long afterOld = oldPoolCommitted();

        long checksum = churned;
        for (int i = 0; i < live.size(); i++) {
            byte[] a = live.get(i);
            checksum += (a[0] & 0xFF) * 31L + (a[CHUNK - 1] & 0xFF);
            intact &= a[0] == (byte) i && a[CHUNK - 1] == (byte) ~i;
        }
        long dropped = (long) (RETAIN - KEEP) * CHUNK;
        boolean shrunk = peak - after >= dropped / 2;
        if (verbose) {
            System.out.println("info (not deterministic): heap committed start=" + start
                    + " peak=" + peak + " after=" + after + " dropped=" + dropped);
            System.out.println("info (not deterministic): old pool committed start=" + startOld
                    + " peak=" + peakOld + " after=" + afterOld);
        }
        if (!intact) {
            System.out.println("retained FAILED: a chunk was corrupted");
        }
        System.out.println("checksum=" + checksum);
        boolean ok = shrunk && intact;
        System.out.println(ok ? "PASS shrunk" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
