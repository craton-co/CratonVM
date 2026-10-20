// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcf f1/o (2026-10-01): a row that reaches the COMPILED refill trigger with a
 * wedged old generation, so {@code CRATONVM_GC_REFILL_TRIGGER_UNJUDGED} can be
 * decided ({@code docs/internal/gc/gcd-d9b-compiled-refill-trigger-cycles-feed-the-overhead-streak-REFUTED-20260928.md}).
 * gcf f7/o: that switch is DELETED (its control never fired); the probe is history.
 *
 * <p>ve2 o-6 found {@code 0} {@code unjudged=refill-trigger} lines on every
 * row of the A/B: the rows it used never latched a streak while the trigger
 * fired, so the arm was never exercised. This probe builds the page's shape on
 * purpose:
 * <ol>
 *   <li>{@code fill}: about 62 MiB (args[1] chunks) of retained {@code long[1024]} chunks,
 *       allocated by a compiled method, so that at {@code -Xmx128m} the
 *       Generational old generation (half the heap by default) is wedged (less
 *       than 2 % of the heap free) once they are promoted;</li>
 *   <li>{@code grow}: a compiled loop that allocates small {@code Cell}s and
 *       keeps all but one cell in {@code args[0]} (default 16; the f1 shape
 *       was 2): about 15 MiB of live growth in 16 MiB of allocation. Every
 *       young cycle in this phase frees about one sixteenth of what was
 *       allocated since the previous one, under 2 % of the heap: on a wedged
 *       old generation each judged trigger cycle is "unproductive" and raises
 *       the GC-overhead streak (the page's defect); unjudged, it does not;</li>
 *   <li>{@code usable}: drop everything, then 24 MiB of fresh allocation.</li>
 * </ol>
 * The live set never exceeds about 78 MiB of a 128 MiB heap, so HotSpot (which
 * has no GC-overhead limit on Serial) completes every phase.
 *
 * <p>HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx128m -cp tools/bench GcfF1oRefillTriggerProbe})
 * prints, in this order, and exits 0:
 * <pre>
 *   fill ok
 *   grow ok kept=300000
 *   usable ok
 *   PASS
 * </pre>
 * A spurious {@code OutOfMemoryError} prints {@code <phase> OutOfMemoryError}
 * then {@code FAIL} (exit 1). Diagnostics go to stderr only.
 *
 * <pre>
 *   javac -d tools/bench tools/bench/GcfF1oRefillTriggerProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx128m -cp tools/bench"
 *   for arm in 0 1; do
 *     CRATONVM_GC_REFILL_TRIGGER_UNJUDGED=$arm CRATONVM_DBG_GC_OVERHEAD=1 CRATONVM_GC_STATS=1 \
 *       timeout 300 cratonvm $P GcfF1oRefillTriggerProbe 2>rt_$arm.err; echo "arm=$arm rc=$?"
 *     grep -c 'refill-trigger: arm=' rt_$arm.err                     # engagement (> 0)
 *     grep -c 'refill-trigger: arm=judged wedged_at_entry=true.*streak=raised' rt_$arm.err
 *   done
 * </pre>
 */
public final class GcfF1oRefillTriggerProbe {
    static final class Cell {
        final long a, b, c, d;

        Cell(long v) {
            this.a = v;
            this.b = v + 1;
            this.c = v + 2;
            this.d = v + 3;
        }
    }

    /**
     * Chunks of {@code long[1024]} (8 KiB of payload each) in {@code fill}:
     * {@code args[1]}, default 7 900 (about 62 MiB).
     *
     * <p>gcf f3/o: the f1 default (7 800, keep one cell in two) never raised
     * the streak (vf1 o-3: 0 {@code streak=raised} lines). The Generational
     * old generation at {@code -Xmx128m} is 64 MiB of a 96 MiB {@code cap}
     * (one 32 MiB semi-space counted), so "wedged" is under 1.92 MiB of old
     * headroom; and a trigger cycle fires on young OCCUPANCY (the floor is
     * {@code live + semi/16} = 2 MiB of new allocation once young holds live
     * data), not every 4 MiB, so with one cell in two dropped each cycle
     * freed about half of a young threshold -- far over 2 % of {@code cap}.
     * The default now drops one cell in {@link #dropEvery} = 16: a cycle on a
     * young generation holding the kept cells frees about 1/16 of 2 MiB,
     * under the sliver, so a judged cycle on the wedged old generation is
     * unproductive and RAISES the streak (the page's defect), and the
     * unjudged arm does not.
     */
    static int fillChunks = 7_900;
    /** Cells kept in {@code grow}. */
    static final int KEPT = 300_000;
    /** Cells per {@code growStep} call (the compiled unit). */
    static final int STEP = 2_000;
    /**
     * {@code args[0]}, default 16: every {@code dropEvery}-th cell of
     * {@code grow} is dropped, the rest kept. {@code 2} is the f1 shape.
     */
    static int dropEvery = 16;

    static long[][] fill;
    static Cell[] kept;
    static long sink;

    /** The compiled fill unit: chunk {@code i}. */
    static long[] fillChunk(int i) {
        final long[] c = new long[1024];
        c[i & 1023] = i;
        return c;
    }

    /**
     * The compiled grow unit: {@code STEP} cells from {@code from}, every
     * second one kept in {@code kept}; returns the next free slot.
     */
    static int growStep(Cell[] keep, int at, long from) {
        int k = at;
        final int drop = dropEvery;
        for (int i = 0; i < STEP && k < keep.length; i++) {
            final Cell c = new Cell(from + i);
            if (i % drop != drop - 1) {
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
        if (args.length > 0) {
            dropEvery = Math.max(2, Integer.parseInt(args[0]));
        }
        if (args.length > 1) {
            fillChunks = Integer.parseInt(args[1]);
        }
        System.err.println("[probe] dropEvery=" + dropEvery + " fillChunks=" + fillChunks);
        String phase = "fill";
        try {
            fill = new long[fillChunks][];
            for (int i = 0; i < fillChunks; i++) {
                fill[i] = fillChunk(i);
            }
            System.out.println("fill ok");

            phase = "grow";
            kept = new Cell[KEPT];
            int at = 0;
            long v = 0;
            while (at < KEPT) {
                at = growStep(kept, at, v);
                v += STEP;
            }
            System.out.println("grow ok kept=" + at);

            phase = "usable";
            fill = null;
            kept = null;
            usable();
            System.out.println("usable ok");
        } catch (OutOfMemoryError e) {
            fill = null;
            kept = null;
            System.err.println("[probe] OutOfMemoryError in phase " + phase + ": " + e.getMessage());
            System.out.println(phase + " OutOfMemoryError");
            System.out.println("FAIL");
            System.exit(1);
        }
        System.out.println("PASS");
    }
}
