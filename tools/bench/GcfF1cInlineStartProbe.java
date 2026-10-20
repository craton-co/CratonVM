// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcf f1/c (2026-10-01): a workload on which the Generational concurrent cycle
 * started by a DIRECT old-gen allocation (`CRATONVM_GEN_CONC_INLINE_START`)
 * can complete, so the switch can be decided
 * ({@code docs/internal/gc/gengc-r4w4-concmark4-the-concurrent-cycle-is-polled-only-after-a-young-collection-20260924.md}).
 *
 * <p>The program allocates only humongous {@code long[]} arrays (placed straight
 * into the old generation at {@code -Xmx512m}), keeps the last {@code keep} of
 * them in a ring held by a LOCAL of a compiled method, and drops the rest. Every
 * dropped array is old garbage that only an old-generation collection
 * reclaims, and no young collection runs. After every allocation the program
 * re-reads every live ring array and checks each sampled word against the value
 * it wrote: a concurrent sweep that freed (and a later allocation that reused)
 * a live array shows as {@code bad>0}. The loop runs in {@link #run}, which
 * OSR-compiles in its first iteration, so the allocations go through the JIT's
 * {@code jit_newarray} helper -- the path whose missing pickup kept the
 * latched start request untaken (gce ve2 c-5).
 *
 * <p>Stdout is a pure function of the arguments and equals HotSpot's:
 * <pre>
 *   inline-start mib=64 keep=2 iters=40 bad=0 checksum=&lt;same on every VM&gt;
 * </pre>
 * The verdict on the switch is on stderr ({@code CRATONVM_DBG=gc-stats}):
 * {@code conccyc_inline_start_taken}, {@code concdrv_cycles_completed} and
 * {@code majcad_majors} / {@code majcad_fallback_start_late}, on against off.
 *
 * <pre>
 *   javac -d tools/bench tools/bench/GcfF1cInlineStartProbe.java
 *   java -XX:+UseSerialGC -Xmx512m -cp tools/bench GcfF1cInlineStartProbe
 *   CRATONVM_GEN_CONC_INLINE_START=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GcfF1cInlineStartProbe
 * </pre>
 *
 * <p>Usage: {@code GcfF1cInlineStartProbe [mib] [keep] [iters]}.
 */
public final class GcfF1cInlineStartProbe {
    static final int STRIDE = 64;

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    static long word(int idx, int j) {
        return (long) idx * 1_000_003L + j;
    }

    /** Returns {@code {bad, checksum}}. */
    static long[] run(int len, int keep, int iters) {
        final long[][] ring = new long[keep][];
        final int[] owner = new int[keep];
        long bad = 0;
        long checksum = 0;
        for (int i = 0; i < iters; i++) {
            long[] a = new long[len];
            for (int j = 0; j < len; j += STRIDE) {
                a[j] = word(i, j);
            }
            a[len - 1] = ~(long) i;
            ring[i % keep] = a;
            owner[i % keep] = i;
            for (int k = 0; k < keep; k++) {
                long[] r = ring[k];
                if (r == null) {
                    continue;
                }
                int idx = owner[k];
                long s = 0;
                for (int j = 0; j < len; j += STRIDE) {
                    long v = r[j];
                    if (v != word(idx, j)) {
                        bad++;
                    }
                    s += v;
                }
                if (r[len - 1] != ~(long) idx) {
                    bad++;
                }
                checksum = checksum * 31 + s;
            }
        }
        return new long[] {bad, checksum};
    }

    public static void main(String[] args) {
        final int mib = arg(args, 0, 64);
        final int keep = arg(args, 1, 2);
        final int iters = arg(args, 2, 40);
        final int len = mib * (1024 * 1024 / 8);
        long[] r = run(len, keep, iters);
        System.out.println("inline-start mib=" + mib + " keep=" + keep + " iters=" + iters
                + " bad=" + r[0] + " checksum=" + r[1]);
    }
}
