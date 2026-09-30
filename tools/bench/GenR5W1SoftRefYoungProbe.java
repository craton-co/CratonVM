// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.SoftReference;

/**
 * gen r5w1/refs5 (2026-09-26): a SoftReference created since the last
 * collection is never cleared by the LRU policy at the next one while the heap
 * has room — HotSpot's `LRUMaxHeapPolicy`.
 *
 * <p>HotSpot stamps a new {@code SoftReference} with the soft CLOCK, which is
 * the time the previous collection ended, and clears one only when
 * {@code clock - timestamp > free_MB_after_last_GC * SoftRefLRUPolicyMSPerMB}.
 * A reference created after the last collection therefore has idle time 0 at
 * the next one, whatever the free space, and is kept.
 *
 * <p>CratonVM's default pre-collection pass (`weakref_null_referents_pre_gc`)
 * compares the wall clock NOW with the creation stamp, against the free space
 * of the generation that just filled (about 0 MB at an allocation-triggered
 * young collection). A soft reference to a YOUNG referent created a few
 * milliseconds before such a collection is condemned and cleared.
 * (`GenR4SoftRefLruProbe` does not show this: its 64 KiB referents are
 * allocated old, and a young collection treats every old object as live.)
 *
 * <p>Each round creates a soft reference to a fresh 256-byte array (young),
 * does not read it, allocates 4 MiB of garbage (so some rounds contain a young
 * collection), and then counts a loss if {@code get()} returns null.
 *
 * <p>Deterministic under HotSpot with the suggested heap: prints
 * <pre>
 *   soft-young rounds=512 lost=0
 *   PASS
 * </pre>
 * Commands:
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W1SoftRefYoungProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W1SoftRefYoungProbe
 *   CRATONVM_SOFTREF_HOTSPOT_LRU=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W1SoftRefYoungProbe
 * </pre>
 * Expected: HotSpot and the {@code CRATONVM_SOFTREF_HOTSPOT_LRU=1} arm print
 * {@code lost=0} and {@code PASS}; the CratonVM default arm is expected to
 * print {@code lost=N} with N &gt; 0 and {@code FAIL} (the divergence
 * `docs/internal/gc/gengc-r4-mark-softref-policy-uses-prefill-free-space-FIXED-20260928.md` describes).
 * Usage: GenR5W1SoftRefYoungProbe [rounds] (default 512, i.e. 2 GiB of garbage).
 */
public final class GenR5W1SoftRefYoungProbe {
    static volatile Object sink;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 512;
        int lost = 0;
        for (int r = 0; r < rounds; r++) {
            SoftReference<byte[]> ref = new SoftReference<>(new byte[256]);
            for (int i = 0; i < 256; i++) {
                sink = new byte[16 * 1024];
            }
            if (ref.get() == null) {
                lost++;
            }
        }
        System.out.println("soft-young rounds=" + rounds + " lost=" + lost);
        System.out.println(lost == 0 ? "PASS" : "FAIL");
        if (lost != 0) {
            System.exit(1);
        }
    }
}
