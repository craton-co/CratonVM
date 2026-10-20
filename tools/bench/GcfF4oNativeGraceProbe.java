// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.ByteBuffer;

/**
 * gcf f4/o (2026-10-01): a row for the FIFTH door of
 * {@code docs/internal/gc/gcf-f1o-latched-grace-ok-is-counted-by-one-door-of-five-FIXED-20261001.md}
 * -- the native factories' {@code NativeContext::reclaim_before_alloc_retry}
 * (counted by {@code note_graced_native_retry} /
 * {@code note_native_alloc_succeeded}).
 *
 * <p>Shape (run at {@code -Xmx128m -Xmn2m}, so a 1.4 MB array cannot live in
 * a young semi-space and goes to the old generation directly):
 * <ol>
 *   <li>{@code drop}: one 2 MiB {@code long[]}, allocated first;</li>
 *   <li>{@code fill}: 1.25 MiB {@code long[]} chunks until the first
 *       {@code OutOfMemoryError} -- the old generation is then wedged, with
 *       less than one chunk of headroom (and, under
 *       {@code CRATONVM_GC_OVERHEAD_LIMIT=1}, the overhead streak latched
 *       after one unproductive collection);</li>
 *   <li>{@code drop = null}, then ONE native allocation of 1.4 MB
 *       ({@code new StringBuilder(int)} or {@code ByteBuffer.allocate}, both
 *       native factories under {@code --compatible}). Its first attempt fails
 *       (the dead 2 MiB is not collected yet); the door's ladder runs a major
 *       that frees the 2 MiB -- under 2 % of the heap, so the streak stays
 *       latched -- and the retry is the GRACED one. It allocates.</li>
 * </ol>
 * HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx128m -Xmn2m -cp tools/bench GcfF4oNativeGraceProbe sb}):
 * <pre>
 *   fill ok
 *   native ok
 *   PASS
 * </pre>
 * Diagnostics on stderr. The VM's verdict is on stderr under
 * {@code CRATONVM_DBG_GC_OVERHEAD=1}: {@code [GC_OVERHEAD] latched-grace: door=native}
 * (the control: the graced retry was taken) and
 * {@code [GC_OVERHEAD] latched-grace-ok: door=native} (it allocated).
 */
public final class GcfF4oNativeGraceProbe {
    static long[] drop;
    static Object[] chain;
    static Object sink;

    public static void main(String[] args) {
        final String mode = args.length > 0 ? args[0] : "sb";
        drop = new long[256 * 1024];
        int chunks = 0;
        try {
            while (true) {
                final Object[] node = new Object[2];
                node[0] = chain;
                node[1] = new long[160 * 1024];
                chain = node;
                chunks++;
            }
        } catch (OutOfMemoryError e) {
            System.err.println("[probe] fill chunks=" + chunks);
        }
        System.out.println("fill ok");
        drop = null;
        try {
            if (mode.equals("bb")) {
                sink = ByteBuffer.allocate(1_400_000);
            } else {
                sink = new StringBuilder(1_400_000);
            }
            System.out.println("native ok");
        } catch (OutOfMemoryError e) {
            chain = null;
            System.out.println("native OutOfMemoryError");
            System.out.println("FAIL");
            System.exit(1);
        }
        chain = null;
        System.out.println("PASS");
    }
}
