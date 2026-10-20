// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcf f5/o (2026-10-01): the positive control of
 * {@code docs/internal/gc/gcd-d9b-compiled-refill-trigger-cycles-feed-the-overhead-streak-REFUTED-20260928.md},
 * which {@code GcfF1oRefillTriggerProbe} failed to fire in vf1 and vf3
 * (gcf f7/o: the switch this decided, {@code CRATONVM_GC_REFILL_TRIGGER_UNJUDGED},
 * is DELETED -- this control did not fire in vf5 either; the probe stays as
 * the d9b page's reproducer)
 * ({@code streak=raised} 0 everywhere; vf3 had not even a
 * {@code wedged_at_entry=true} line).
 *
 * <p>Why that probe could not fire: a judged young cycle is "unproductive"
 * only when the old generation is wedged (headroom under 2 % of the heap)
 * AND the cycle freed under 2 % of the heap. With the default young
 * generation (two 32 MiB semi-spaces at {@code -Xmx128m}) a young cycle frees
 * megabytes, and the probe's fill was promoted gradually, so the old
 * generation was seldom wedged when a trigger cycle ran.
 *
 * <p>This probe runs at {@code -Xmx128m -Xmn2m}: a young cycle can free at
 * most one small semi-space -- always under 2 % of the heap -- and the fill
 * goes to the old generation DIRECTLY (1.25 MiB chunks, larger than a
 * semi-space):
 * <ol>
 *   <li>{@code fill}: 1.25 MiB {@code long[]} chunks until the first
 *       {@code OutOfMemoryError}, then the last two chunks are dropped -- the
 *       old generation keeps 1.25-3.75 MiB of headroom on any collector, so
 *       the next phase fits on HotSpot, while here it is wedged as soon as the
 *       kept cells are promoted into it;</li>
 *   <li>{@code grow}: a compiled loop allocating {@code args[1]} small cells
 *       (default 400 000, about 19 MiB) and keeping {@code args[0]} of them
 *       (default 20 000, about 1 MiB live). Every young cycle in this phase
 *       is a judged trigger cycle on a wedged old generation that frees less
 *       than 2 % of the heap: {@code streak=raised} on the judged arm (the
 *       page's defect), none on {@code CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=1};</li>
 *   <li>{@code usable}: drop everything, 24 MiB of fresh allocation.</li>
 * </ol>
 * Run the control row with {@code CRATONVM_GC_OVERHEAD_LIMIT=1000} so the
 * raised streak never ends the run (the control is the raise itself); the
 * default limit row shows the defect's Java-visible effect (a spurious
 * {@code grow OutOfMemoryError} on the judged arm).
 *
 * <p>HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx128m -Xmn2m -cp tools/bench GcfF5oRefillTriggerWedgeProbe}):
 * <pre>
 *   fill ok
 *   grow ok kept=20000
 *   usable ok
 *   PASS
 * </pre>
 * Diagnostics on stderr only.
 */
public final class GcfF5oRefillTriggerWedgeProbe {
    static final class Cell {
        final long a, b, c, d;

        Cell(long v) {
            this.a = v;
            this.b = v + 1;
            this.c = v + 2;
            this.d = v + 3;
        }
    }

    static Object[] chain;
    static Cell[] kept;
    static long sink;
    static final int STEP = 2_000;

    /** The compiled grow unit: {@code STEP} cells; every {@code every}-th kept. */
    static int growStep(Cell[] keep, int at, long from, int every) {
        int k = at;
        for (int i = 0; i < STEP; i++) {
            final Cell c = new Cell(from + i);
            if (k < keep.length && (from + i) % every == 0) {
                keep[k++] = c;
            } else {
                sink += c.d;
            }
        }
        return k;
    }

    static boolean usable() {
        long s = 0;
        for (int i = 0; i < 24; i++) {
            final long[] chunk = new long[128 * 1024];
            chunk[i] = i;
            s += chunk[i];
        }
        sink += s;
        return true;
    }

    public static void main(String[] args) {
        final int keepN = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        final long total = args.length > 1 ? Long.parseLong(args[1]) : 400_000L;
        final int every = (int) Math.max(1, total / keepN);
        String phase = "fill";
        int chunks = 0;
        try {
            try {
                while (true) {
                    final Object[] node = new Object[2];
                    node[0] = chain;
                    node[1] = new long[160 * 1024];
                    chain = node;
                    chunks++;
                }
            } catch (OutOfMemoryError e) {
                // Drop the last two chunks: the margin the grow phase needs.
                for (int i = 0; i < 2 && chain != null; i++) {
                    chain = (Object[]) chain[0];
                }
            }
            System.err.println("[probe] fill chunks=" + chunks + " keep=" + keepN + " total=" + total);
            System.out.println("fill ok");

            phase = "grow";
            kept = new Cell[keepN];
            int at = 0;
            for (long v = 0; v < total; v += STEP) {
                at = growStep(kept, at, v, every);
            }
            System.out.println("grow ok kept=" + at);

            phase = "usable";
            chain = null;
            kept = null;
            usable();
            System.out.println("usable ok");
        } catch (OutOfMemoryError e) {
            chain = null;
            kept = null;
            System.err.println("[probe] OutOfMemoryError in phase " + phase + ": " + e.getMessage());
            System.out.println(phase + " OutOfMemoryError");
            System.out.println("FAIL");
            System.exit(1);
        }
        System.out.println("PASS");
    }
}
