// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.ByteBuffer;

/**
 * gcg g1/o (2026-10-01): a row whose native-door grace control FIRES, for
 * {@code docs/internal/gc/gcf-f1o-latched-grace-ok-is-counted-by-one-door-of-five-FIXED-20261001.md}.
 *
 * <p>{@code GcfF4oNativeGraceProbe} (gcf f4/o) printed and concatenated
 * between the fill's {@code OutOfMemoryError} and its native allocation, so
 * on its {@code CRATONVM_GC_OVERHEAD_LIMIT=1} rows the interpreter's string
 * door met the latched streak first and spent the episode's grace there
 * ({@code interp_enter=1}); the native door's control line never printed (vf4
 * o4-1, 0/6). Here NOTHING allocates between the fill's error and the native
 * allocation: the fill ends in a {@code catch} that only sets a flag, the
 * 2 MiB array is dropped, and the very next allocation is the native one.
 *
 * <p>Shape (run at {@code -Xmx128m -Xmn2m}: a 1.4 MB array cannot live in a
 * young semi-space and goes to the old generation directly):
 * <ol>
 *   <li>{@code drop}: one 1.375 MiB {@code long[]};</li>
 *   <li>{@code fill}: 1.25 MiB, then 1.125 MiB, {@code long[]} chunks until the
 *       {@code OutOfMemoryError} (the old generation is wedged);</li>
 *   <li>{@code drop = null}; ONE allocation of 1.4 MB: {@code sb} =
 *       {@code new StringBuilder(int)}, {@code bb} = {@code ByteBuffer.allocate}
 *       (native factories under {@code --compatible}; bytecode under
 *       {@code --jdk-only}). Under {@code CRATONVM_GC_OVERHEAD_LIMIT=1} the
 *       door's ladder runs a major that frees the 1.375 MiB -- under 2 % of the
 *       heap, so the streak stays latched -- and the retry is the GRACED one.</li>
 * </ol>
 * HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx128m -Xmn2m -cp tools/probes GcgO1NativeGraceProbe sb}):
 * <pre>
 *   fill ok
 *   native ok
 *   PASS
 * </pre>
 * Every stdout line is a constant built before the fill. Diagnostics on
 * stderr; the VM's witness is {@code [GC_OVERHEAD] latched-grace: door=native}
 * and {@code latched-grace-ok: door=native} under
 * {@code CRATONVM_DBG_GC_OVERHEAD=1}, and {@code ladder_latched_grace_ok=} on
 * the {@code [GC] oome_ladder:} line under {@code CRATONVM_GC_STATS=1}.
 */
public final class GcgO1NativeGraceProbe {
    static long[] drop;
    static Object[] chain;
    static Object sink;

    static final String FILL_OK = "fill ok";
    static final String FILL_NONE = "fill never failed";
    static final String NATIVE_OK = "native ok";
    static final String NATIVE_OOME = "native oome";
    static final String PASS = "PASS";
    static final String FAIL = "FAIL";

    static volatile int chunks;
    static volatile boolean filled;

    /** The fill, on its own thread (see the class comment). */
    static void fill() {
        int n = 0;
        try {
            while (true) {
                final Object[] node = new Object[2];
                node[0] = chain;
                node[1] = new long[160 * 1024];
                chain = node;
                n++;
            }
        } catch (OutOfMemoryError e) {
            filled = true;
        }
        // gcg g1b/o: a second stage of 1.125 MiB chunks (old generation directly) leaves under
        // 1.125 MiB of old headroom, so headroom plus the dropped 1.375 MiB stays under
        // 2 % of the heap (2.66 MB) and the old generation stays wedged: the door then
        // meets a latched streak. With the first stage alone the headroom (up to
        // 1.25 MiB) plus the drop crossed 2 % and the streak reset (vg1: the
        // control never fired).
        try {
            while (true) {
                final Object[] node = new Object[2];
                node[0] = chain;
                node[1] = new long[144 * 1024];
                chain = node;
                n++;
            }
        } catch (OutOfMemoryError e) {
            filled = true;
        }
        chunks = n;
    }

    public static void main(String[] args) throws InterruptedException {
        final boolean sb = args.length == 0 || !"bb".equals(args[0]);
        // Warm both streams' write paths before the heap is full; prints nothing.
        System.out.print("");
        System.err.print("");
        drop = new long[176 * 1024];
        // The fill raises its OutOfMemoryError on ANOTHER thread: the
        // Generational post-OOME native debt (`native_call_owes_oome_major`)
        // is paid only by the raising thread, so main's native door below
        // meets the latched streak with nothing paid ahead of it.
        final Thread filler = new Thread(GcgO1NativeGraceProbe::fill, "gcg-o1-fill");
        filler.start();
        filler.join();
        drop = null;
        boolean ok;
        try {
            if (sb) {
                sink = new StringBuilder(1_400_000);
            } else {
                sink = ByteBuffer.allocate(1_400_000);
            }
            ok = true;
        } catch (OutOfMemoryError e) {
            ok = false;
        }
        chain = null;
        sink = null;
        System.out.println(filled ? FILL_OK : FILL_NONE);
        System.out.println(ok ? NATIVE_OK : NATIVE_OOME);
        System.out.println(filled && ok ? PASS : FAIL);
        System.err.println("[probe] mode=" + (sb ? "sb" : "bb") + " fill chunks=" + chunks);
    }
}
