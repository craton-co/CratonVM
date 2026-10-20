// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zs s6/no (2026-10-15): do soft references survive collections on a nearly
// full heap that never throws OutOfMemoryError?
//
// HotSpot clears a softly reachable object when it has not been touched for
// longer than SoftRefLRUPolicyMSPerMB (1000 ms) per MiB of free heap, so on a
// heap with little free space every collection clears the idle ones, and its
// ZGC also clears all of them on an allocation-stall collection. This VM
// clears them only on the last-ditch rung before an OutOfMemoryError. The
// probe fills a soft cache, then a live set to `fillPct` of -Xmx (no OOME),
// idles past the LRU interval, churns garbage so the collector runs, and
// counts the live soft entries.
//
// stdout (deterministic on HotSpot ZGC; compare):
//   zs6no soft-near-full fill=<pct> soft_alive=<bucket> oome=false
// where <bucket> is none | some | all.
// stderr: the exact count and collections.
//
// Run: cratonvm -Xmx256m -cp tools/probes Zs6NoSoftNearFull 90
//      java -XX:+UseZGC -Xmx256m -cp tools/probes Zs6NoSoftNearFull 90
// Loop counters are `int` stepping by 1 on purpose, as elsewhere in this kit.
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.SoftReference;
import java.util.ArrayList;
import java.util.List;

public class Zs6NoSoftNearFull {
    static volatile Object sink;

    static long collections() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            long c = b.getCollectionCount();
            if (c > 0) {
                n += c;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        int fillPct = args.length > 0 ? Integer.parseInt(args[0]) : 90;
        long max = Runtime.getRuntime().maxMemory();
        List<SoftReference<byte[]>> cache = new ArrayList<>();
        for (int i = 0; i < 64; i++) {
            cache.add(new SoftReference<>(new byte[64 * 1024]));
        }
        List<int[]> live = new ArrayList<>();
        boolean oome = false;
        try {
            long target = max / 100 * fillPct;
            long have = 4L << 20;
            while (have < target) {
                live.add(new int[16 * 1024]);
                have += 64 * 1024 + 16;
            }
            // Idle past the LRU interval (1000 ms per free MiB: a few MiB free).
            Thread.sleep(3000);
            long gc0 = collections();
            for (int i = 0; i < 2_000_000 && collections() - gc0 < 8; i++) {
                sink = new byte[256];
            }
        } catch (OutOfMemoryError e) {
            oome = true;
        }
        int alive = 0;
        for (SoftReference<byte[]> s : cache) {
            if (s.get() != null) {
                alive++;
            }
        }
        String bucket = alive == 0 ? "none" : alive == cache.size() ? "all" : "some";
        System.err.println("zs6no soft-near-full alive=" + alive + "/" + cache.size()
                + " live_arrays=" + live.size() + " gcs=" + collections());
        System.out.println("zs6no soft-near-full fill=" + fillPct + " soft_alive=" + bucket
                + " oome=" + oome);
    }
}
