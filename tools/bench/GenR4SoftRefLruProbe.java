// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.SoftReference;

/**
 * gen r4/mark (2026-09-23): SoftReferences must survive ordinary young
 * collections while the heap has room, as HotSpot's LRU policy guarantees.
 *
 * <p>HotSpot (default {@code -XX:SoftRefLRUPolicyMSPerMB=1000}) clears a soft
 * reference only when {@code clock - timestamp > free_MB_after_last_GC * 1000}
 * ms, where {@code clock} is the time of the PREVIOUS collection and
 * {@code get()} stamps {@code timestamp = clock}. So:
 * <ul>
 *   <li>a reference read since the last GC has idle time 0 and is never
 *       cleared by the LRU policy, whatever the free space;</li>
 *   <li>an unread reference in a heap with ~200 MB free after GC is kept for
 *       ~200 s.</li>
 * </ul>
 * CratonVM's pre-GC pass (`weakref_null_referents_pre_gc`) evaluates the
 * policy with the free space of the generation that just FILLED (≈0 MB at an
 * allocation-triggered young GC) against the current wall clock, so both
 * references below are expected to be cleared within the first few young
 * collections — soft references then behave like weak ones.
 *
 * <p>Deterministic under HotSpot with the suggested heap: prints two
 * {@code ok} lines and {@code PASS}.
 * <pre>
 *   java -Xmx512m -cp tools/bench GenR4SoftRefLruProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4SoftRefLruProbe
 * </pre>
 * Usage: GenR4SoftRefLruProbe [allocMB] (default 2048 MB of short-lived garbage).
 * See docs/internal/gaps/gengc-r4-mark-softref-policy-uses-prefill-free-space-20260923.md.
 */
public final class GenR4SoftRefLruProbe {
    static volatile Object sink;

    public static void main(String[] args) {
        long allocMb = args.length > 0 ? Long.parseLong(args[0]) : 2048;
        SoftReference<byte[]> touched = new SoftReference<>(new byte[64 * 1024]);
        SoftReference<byte[]> untouched = new SoftReference<>(new byte[64 * 1024]);
        long chunks = allocMb * 16; // 64 KiB chunks
        boolean touchedLost = false;
        long lostAt = -1;
        for (long i = 0; i < chunks; i++) {
            sink = new byte[64 * 1024];
            if (touched.get() == null && !touchedLost) {
                touchedLost = true;
                lostAt = i;
            }
        }
        int failures = 0;
        failures += check("touched-soft-ref-survives-young-gcs", !touchedLost,
                touchedLost ? "lost after " + (lostAt / 16) + " MB" : "");
        failures += check("untouched-soft-ref-survives-with-free-heap", untouched.get() != null, "");
        System.out.println(failures == 0 ? "PASS" : "FAIL " + failures);
        if (failures != 0) System.exit(1);
    }

    static int check(String name, boolean ok, String detail) {
        System.out.println(name + ": " + (ok ? "ok" : "FAILED " + detail));
        return ok ? 0 : 1;
    }
}
