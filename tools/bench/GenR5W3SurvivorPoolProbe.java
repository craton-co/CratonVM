// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;

/**
 * gen r5w3/obs7 (2026-09-26): after a young collection {@code Survivor Space}
 * holds what survived it and {@code Eden Space} is empty, as on HotSpot
 * Serial. Page:
 * {@code docs/internal/gc/gengc-r5w2-obs6-proposal-survivor-pool-reports-the-last-cycles-survivors-IMPLEMENTED-20260927.md}.
 *
 * <p>A {@code System.gc()} first moves everything already live out of the
 * young generation; then {@code R} KiB (default 2048) of small arrays are
 * allocated and kept, and garbage is allocated until the young collector
 * ({@code Copy}) has run once more. The pools' COLLECTION usage (the usage
 * that collection left) must show the kept bytes in {@code Survivor Space}
 * and (almost) nothing in {@code Eden Space}.
 *
 * <p>Deterministic stdout on HotSpot Serial (the young generation must be
 * large enough for a survivor space to hold {@code R} KiB: {@code -Xmn64m}
 * gives 6.4 MiB survivors):
 * <pre>
 *   survivor-holds-retained=true
 *   eden-empty-after-young-gc=true
 *   survivor-used-now-holds-retained=true
 * </pre>
 * Before gen r5w3/obs7 CratonVM printed {@code false} for the first and last
 * lines ({@code Survivor Space} was always {@code 0}) and {@code false} for
 * the second (the survivors were counted in {@code Eden Space}). Numbers go to
 * stderr. Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -Xmn64m -cp tools/bench GenR5W3SurvivorPoolProbe 2048
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -Xmn64m -cp tools/bench GenR5W3SurvivorPoolProbe 2048
 * </pre>
 */
public final class GenR5W3SurvivorPoolProbe {
    static volatile Object keepSink;
    static volatile Object garbageSink;

    public static void main(String[] args) {
        int retainKb = args.length > 0 ? Integer.parseInt(args[0]) : 2048;
        MemoryPoolMXBean eden = null;
        MemoryPoolMXBean survivor = null;
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            if (p.getName().equals("Eden Space")) {
                eden = p;
            } else if (p.getName().equals("Survivor Space")) {
                survivor = p;
            }
        }
        GarbageCollectorMXBean young = null;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            if (gc.getName().equals("Copy")) {
                young = gc;
            }
        }
        if (eden == null || survivor == null || young == null) {
            System.out.println("FAIL not Serial's beans");
            return;
        }

        System.gc();
        List<byte[]> keep = new ArrayList<>(retainKb);
        for (int i = 0; i < retainKb; i++) {
            keep.add(new byte[1000]);
        }
        keepSink = keep;
        long retained = retainKb * 1000L;

        long before = young.getCollectionCount();
        while (young.getCollectionCount() == before) {
            for (int i = 0; i < 1024; i++) {
                garbageSink = new byte[512];
            }
        }
        MemoryUsage s = survivor.getCollectionUsage();
        MemoryUsage e = eden.getCollectionUsage();
        MemoryUsage sNow = survivor.getUsage();
        System.err.println("[probe] retained=" + retained
            + " survivor_collection_used=" + s.getUsed()
            + " eden_collection_used=" + e.getUsed()
            + " survivor_used_now=" + sNow.getUsed()
            + " young_collections=" + (young.getCollectionCount() - before)
            + " kept=" + ((List<?>) keepSink).size());
        System.out.println("survivor-holds-retained=" + (s.getUsed() >= retained * 9 / 10));
        System.out.println("eden-empty-after-young-gc=" + (e.getUsed() < retained / 10));
        System.out.println("survivor-used-now-holds-retained=" + (sNow.getUsed() >= retained * 9 / 10));
    }
}
