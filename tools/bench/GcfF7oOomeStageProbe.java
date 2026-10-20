// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcf f7/o (2026-10-01): WHERE {@link GcfF3oOomeShapeProbe} dies, for
 * {@code docs/internal/gc/gcf-f5o-a-vm-raised-oome-whose-construction-fails-escapes-every-catch-FIXED-20261001.md}.
 *
 * <p>The same fill and the same eight caught errors as the shape probe. Then,
 * after {@code chain = null}, the shape probe's post-fill section runs inside
 * a {@code catch (OutOfMemoryError)} that records which statement threw. On
 * HotSpot nothing throws there: the first allocation after the drop collects
 * the dropped chain. Then {@code System.gc()} and one 8 MiB allocation decide
 * whether the chain was still reachable.
 *
 * <p>Every stdout line is a string built before the fill (a static table), so
 * the report itself cannot fail on a full heap.
 *
 * HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench GcfF7oOomeStageProbe}):
 * <pre>
 *   caught=8
 *   post-null-oome-at=none
 *   big-alloc-after-gc=ok
 *   empty-trace-all=true
 *   suppressed-max=0
 * </pre>
 * Readings:
 * <ul>
 *   <li>{@code post-null-oome-at=getStackTrace} (or another stage) with
 *       {@code big-alloc-after-gc=ok}: the chain was garbage, and the door that
 *       threw did not collect (the native one-shot allocation shape);</li>
 *   <li>{@code big-alloc-after-gc=oome}: the chain was still reachable after
 *       {@code chain = null} and a full collection (a stale root);</li>
 * </ul>
 * Diagnostics on stderr only.
 */
public final class GcfF7oOomeStageProbe {
    static final Throwable MARK = new Throwable("mark");
    static Object[] chain;
    static long[] big;

    static final String[] CAUGHT = {
        "caught=0", "caught=1", "caught=2", "caught=3", "caught=4",
        "caught=5", "caught=6", "caught=7", "caught=8",
    };
    static final String[] STAGE = {
        "post-null-oome-at=none",
        "post-null-oome-at=getStackTrace",
        "post-null-oome-at=addSuppressed",
        "post-null-oome-at=getSuppressed",
    };
    static final String BIG_OK = "big-alloc-after-gc=ok";
    static final String BIG_OOME = "big-alloc-after-gc=oome";
    static final String EMPTY_TRUE = "empty-trace-all=true";
    static final String EMPTY_FALSE = "empty-trace-all=false";
    static final String[] SUPPRESSED = {
        "suppressed-max=0", "suppressed-max=1", "suppressed-max=2", "suppressed-max=3",
        "suppressed-max=4", "suppressed-max=5", "suppressed-max=6", "suppressed-max=7",
        "suppressed-max=8", "suppressed-max=more",
    };

    public static void main(String[] args) {
        // Warm both streams' write paths before the heap is full; prints nothing.
        System.out.print("");
        System.err.print("");
        final OutOfMemoryError[] caught = new OutOfMemoryError[8];
        int fillOomes = 0;
        int size = 1 << 20;
        while (size >= 2) {
            try {
                final Object[] node = new Object[2];
                node[0] = chain;
                node[1] = new long[size];
                chain = node;
            } catch (OutOfMemoryError e) {
                fillOomes++;
                size >>= 1;
            }
        }
        int n = 0;
        for (int i = 0; i < caught.length; i++) {
            try {
                chain = new Object[] {chain, new long[1 << 16]};
            } catch (OutOfMemoryError e) {
                caught[i] = e;
                n++;
            }
        }
        chain = null;
        boolean emptyAll = n > 0;
        int suppressedMax = 0;
        int stage = 0;
        int failedAt = 0;
        try {
            for (OutOfMemoryError e : caught) {
                if (e == null) {
                    continue;
                }
                stage = 1;
                if (e.getStackTrace().length != 0) {
                    emptyAll = false;
                }
                stage = 2;
                e.addSuppressed(MARK);
                stage = 3;
                suppressedMax = Math.max(suppressedMax, e.getSuppressed().length);
            }
        } catch (OutOfMemoryError post) {
            failedAt = stage;
        }
        System.gc();
        boolean bigOk;
        try {
            big = new long[1 << 20];
            bigOk = true;
        } catch (OutOfMemoryError e) {
            bigOk = false;
        }
        big = null;
        System.out.println(CAUGHT[n]);
        System.out.println(STAGE[failedAt]);
        System.out.println(bigOk ? BIG_OK : BIG_OOME);
        System.out.println(emptyAll ? EMPTY_TRUE : EMPTY_FALSE);
        System.out.println(SUPPRESSED[Math.min(suppressedMax, SUPPRESSED.length - 1)]);
        try {
            Runtime rt = Runtime.getRuntime();
            System.err.println("[probe] fill-oomes=" + fillOomes + " used-after-gc="
                    + (rt.totalMemory() - rt.freeMemory()));
        } catch (OutOfMemoryError e) {
            System.err.println("[probe] diagnostics line: OutOfMemoryError");
        }
    }
}
