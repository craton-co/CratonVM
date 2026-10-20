// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryType;

/**
 * gen r4w6/young6 (2026-09-24): ADAPTIVE TENURING — do medium-lived objects die
 * young, or are they promoted to die in the old generation?
 *
 * <p>The workload:
 * <ul>
 *   <li>a LONG-LIVED CORE of 4096 records (~1 MiB), kept for the whole run;</li>
 *   <li>MEDIUM-LIVED records: 16 per iteration, each living exactly
 *       {@code life} iterations (a ring of {@code 16 * life} slots);</li>
 *   <li>SHORT-LIVED garbage: 1 MiB per iteration ({@code long[254]} x 512),
 *       which drives the young collections.</li>
 * </ul>
 * At the default {@code -Xmx256m} on CratonVM's generational collector the
 * young trigger is ~32 MiB, so a young collection runs every ~32 iterations and
 * a medium record ({@code life = 128}) survives 3 to 4 young collections, then
 * dies. With the historical FIXED promotion age (3 survivals) every such record
 * is tenured on its third survival and dies in old gen; with HotSpot-style
 * adaptive tenuring (threshold from the age table, ceiling 15) the medium set
 * (~512 KiB live) is far below the survivor target and dies young.
 *
 * <p>The probe measures old-gen USED growth (from the {@code MemoryPoolMXBean}
 * whose name contains "Old" or "Tenured") across the measured window, after a
 * warm-up long enough for the core to be tenured under either policy, and
 * compares it with the medium records allocated in the window. It also counts
 * old-generation collections in the window (collectors named "Old",
 * "MarkSweep", "Tenured" or "major"): a major in the window reclaims promoted
 * garbage and makes the growth a lower bound, and the verdict says so.
 *
 * <p>Output: one DETERMINISTIC line, then one LABELLED measurement line (its
 * numbers depend on the collector and are not a checksum):
 * <pre>
 *   CHECKSUM tenuring warmup=1024 iters=8192 life=128 checksum=1741697103872 corrupt=0
 *   VERDICT tenuring medium-lived=&lt;died-young|partial|promoted&gt; old-growth-pct=&lt;n&gt; ...
 * </pre>
 * {@code checksum} is {@code 11 * (sum of every verified id)}: the 4096 core ids
 * (1..4096) plus {@code (warmup + iters) * 16} medium ids starting at
 * 1,000,001 — each verified exactly once. For the defaults that is
 * {@code 11 * (8,390,656 + 158,327,709,696) = 1741697103872}.
 *
 * <p>Expected verdicts:
 * <ul>
 *   <li>HotSpot 25, any collector, {@code -Xmx256m}:
 *       {@code medium-lived=died-young} (adaptive tenuring, threshold 15);</li>
 *   <li>CratonVM {@code -XX:+UseGenerationalGC -Xmx256m}, default flags:
 *       {@code medium-lived=promoted} (fixed age 3);</li>
 *   <li>CratonVM with {@code CRATONVM_GC_ADAPTIVE_TENURING=1} (or
 *       {@code -XX:MaxTenuringThreshold=15}): {@code medium-lived=died-young}.</li>
 * </ul>
 * Commands:
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4W6TenuringProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W6TenuringProbe
 *   CRATONVM_GC_ADAPTIVE_TENURING=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W6TenuringProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -XX:MaxTenuringThreshold=15 -XX:+PrintTenuringDistribution -cp tools/bench GenR4W6TenuringProbe
 * </pre>
 * Optional args: {@code warmup iters life}. Exit status 1 on a corrupt record.
 */
public final class GenR4W6TenuringProbe {
    static final int CORE = 4096;
    static final int MEDIUM_PER_ITER = 16;
    static final long MEDIUM_ID_BASE = 1_000_000L;
    static final int GARBAGE_ARRAYS_PER_ITER = 512;

    static final class Rec {
        final long id;
        final long[] data;

        Rec(long id) {
            this.id = id;
            this.data = new long[26]; // ~208 B + headers: ~256 B per record
            for (int k = 0; k < data.length; k++) {
                data[k] = id * (k + 7);
            }
        }

        boolean intact() {
            for (int k = 0; k < data.length; k++) {
                if (data[k] != id * (k + 7)) {
                    return false;
                }
            }
            return true;
        }
    }

    static volatile Object sink;

    static MemoryPoolMXBean oldPool() {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            final String n = p.getName();
            if (p.getType() == MemoryType.HEAP && (n.contains("Old") || n.contains("Tenured"))) {
                return p;
            }
        }
        return null;
    }

    static long oldCollections() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            final String name = b.getName();
            if (name.contains("Old") || name.contains("MarkSweep") || name.contains("Tenured")
                    || name.contains("major")) {
                n += Math.max(0, b.getCollectionCount());
            }
        }
        return n;
    }

    public static void main(String[] args) {
        final int warmup = args.length > 0 ? Integer.parseInt(args[0]) : 1024;
        final int iters = args.length > 1 ? Integer.parseInt(args[1]) : 8192;
        final int life = args.length > 2 ? Integer.parseInt(args[2]) : 128;

        long checksum = 0;
        long corrupt = 0;

        final Rec[] core = new Rec[CORE];
        for (int i = 0; i < CORE; i++) {
            core[i] = new Rec(i + 1);
        }

        final Rec[] ring = new Rec[MEDIUM_PER_ITER * life];
        long nextId = MEDIUM_ID_BASE + 1;
        long slotCursor = 0;

        final MemoryPoolMXBean old = oldPool();
        long oldBaseline = -1;
        long oldGcsBaseline = 0;

        for (int i = 0; i < warmup + iters; i++) {
            if (i == warmup) {
                oldBaseline = old == null ? -1 : old.getUsage().getUsed();
                oldGcsBaseline = oldCollections();
            }
            for (int g = 0; g < GARBAGE_ARRAYS_PER_ITER; g++) {
                sink = new long[254];
            }
            for (int m = 0; m < MEDIUM_PER_ITER; m++) {
                final int slot = (int) (slotCursor++ % ring.length);
                final Rec prev = ring[slot];
                if (prev != null) {
                    if (!prev.intact()) {
                        corrupt++;
                    }
                    checksum += prev.id + prev.data[3];
                }
                ring[slot] = new Rec(nextId++);
            }
        }
        final long oldAfter = old == null ? -1 : old.getUsage().getUsed();
        final long oldGcs = oldCollections() - oldGcsBaseline;

        for (Rec r : ring) {
            if (r != null) {
                if (!r.intact()) {
                    corrupt++;
                }
                checksum += r.id + r.data[3];
            }
        }
        for (Rec r : core) {
            if (!r.intact()) {
                corrupt++;
            }
            checksum += r.id + r.data[3];
        }
        sink = null;

        System.out.println("CHECKSUM tenuring warmup=" + warmup + " iters=" + iters + " life=" + life
                + " checksum=" + checksum + " corrupt=" + corrupt);

        // Medium bytes allocated in the window, at ~256 B per record (the
        // estimate the percentage is taken against; labelled as such).
        final long mediumWindowBytes = (long) iters * MEDIUM_PER_ITER * 256L;
        if (old == null || oldBaseline < 0) {
            System.out.println("VERDICT tenuring medium-lived=unmeasured (no old-generation memory pool)");
        } else {
            final long growth = Math.max(0, oldAfter - oldBaseline);
            final long pct = mediumWindowBytes == 0 ? 0 : growth * 100 / mediumWindowBytes;
            final String v = pct < 25 ? "died-young" : pct < 75 ? "partial" : "promoted";
            System.out.println("VERDICT tenuring medium-lived=" + v + " old-growth-pct=" + pct
                    + " [measured, collector-dependent: old-pool=" + old.getName()
                    + " old-used-baseline=" + oldBaseline + " old-used-after=" + oldAfter
                    + " medium-window-bytes~=" + mediumWindowBytes
                    + " old-gcs-in-window=" + oldGcs
                    + (oldGcs > 0 ? " (a major ran in the window; growth is a lower bound)" : "")
                    + "]");
        }
        if (corrupt != 0) {
            System.exit(1);
        }
    }
}
