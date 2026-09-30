// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.ReferenceQueue;
import java.lang.ref.WeakReference;

/**
 * gen r4/mark (2026-09-23): an UNREACHABLE {@code Reference} object is ordinary
 * garbage, even while its referent is alive.
 *
 * <p>CratonVM roots every weak/soft/phantom {@code Reference} object whose
 * processor entry is not yet cleared (`vm/src/memory/roots.rs` step 22,
 * `ReferenceProcessor::pending_reference_object_addresses`). An entry is
 * cleared only when its REFERENT dies. So a dropped {@code WeakReference} to a
 * long-lived object — a removed {@code WeakHashMap} entry, a
 * {@code ThreadLocalMap$Entry} after {@code ThreadLocal.remove()}, a listener
 * wrapper — is retained, with everything it references, for as long as the
 * referent lives.
 *
 * <p>This probe drops 20 000 weak references to ONE long-lived key, each
 * carrying 64 KiB of payload (~1.25 GiB in total). HotSpot collects every one
 * of them; with the rooting above they are all retained and a 256 MiB heap
 * runs out.
 *
 * <p>Deterministic under HotSpot: prints {@code PASS}.
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4DroppedReferenceLeakProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4DroppedReferenceLeakProbe
 * </pre>
 * <p>2026-09-23 wave 3: the gc-common round (w1-d F1) stopped rooting
 * QUEUE-LESS references, so the default mode now passes. A second argument
 * {@code queued} registers every {@code Entry} with one shared
 * {@code ReferenceQueue} — the {@code WeakHashMap.Entry} shape — which is
 * still rooted while its referent lives. HotSpot prints {@code PASS} in both
 * modes:
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4DroppedReferenceLeakProbe 20000 queued
 * </pre>
 * See docs/internal/gaps/gengc-r4-mark-uncleared-reference-objects-are-roots-FIXED-20260927.md.
 */
public final class GenR4DroppedReferenceLeakProbe {
    static final Object KEY = new Object(); // lives for the whole run
    static final ReferenceQueue<Object> QUEUE = new ReferenceQueue<>();

    static final class Entry extends WeakReference<Object> {
        final byte[] payload = new byte[64 * 1024];

        Entry(Object referent) {
            super(referent);
        }

        Entry(Object referent, ReferenceQueue<Object> q) {
            super(referent, q);
        }
    }

    static volatile Entry sink;

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        boolean queued = args.length > 1 && args[1].equals("queued");
        try {
            for (int i = 0; i < n; i++) {
                // the previous Entry becomes garbage here
                sink = queued ? new Entry(KEY, QUEUE) : new Entry(KEY);
            }
        } catch (OutOfMemoryError e) {
            sink = null;
            System.out.println("FAIL: OutOfMemoryError — dropped References were retained");
            System.exit(1);
        }
        System.out.println("PASS");
    }
}
