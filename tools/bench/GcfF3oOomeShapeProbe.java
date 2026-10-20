// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcf f3/o (2026-10-01): the shape of a VM-raised {@code OutOfMemoryError} on
 * a full heap -- {@code docs/internal/gc/gcf-f2o-preallocated-oome-defaults-carry-a-boot-stack-trace-and-mutable-state-FIXED-20261001.md}.
 *
 * <p>Fills the heap down to 16-byte holes (dozens of OOMEs on the way, so
 * HotSpot's pool of four traced copies is spent), then catches eight more
 * OOMEs while the heap is still full, releases the fill, and reports:
 * <ul>
 *   <li>{@code empty-trace-all}: every one of the eight has an empty stack
 *       trace -- HotSpot hands out its message default, allocated without the
 *       constructor (no backtrace);</li>
 *   <li>{@code suppressed-max}: after {@code addSuppressed(MARK)} on each, the
 *       largest {@code getSuppressed().length} -- {@code 0} on HotSpot (a
 *       VM-raised OOME has {@code suppressedExceptions == null}, so the call is
 *       a no-op); a shared, constructor-built instance accumulates up to 8.</li>
 * </ul>
 * HotSpot 25 ({@code java -XX:+UseSerialGC -Xmx64m -cp tools/bench GcfF3oOomeShapeProbe}):
 * <pre>
 *   caught=8
 *   empty-trace-all=true
 *   suppressed-max=0
 * </pre>
 * Diagnostics on stderr only.
 */
public final class GcfF3oOomeShapeProbe {
    static final Throwable MARK = new Throwable("mark");
    static Object[] chain;

    public static void main(String[] args) {
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
        for (OutOfMemoryError e : caught) {
            if (e == null) {
                continue;
            }
            if (e.getStackTrace().length != 0) {
                emptyAll = false;
            }
            e.addSuppressed(MARK);
            suppressedMax = Math.max(suppressedMax, e.getSuppressed().length);
        }
        System.err.println("[probe] fill-oomes=" + fillOomes);
        System.out.println("caught=" + n);
        System.out.println("empty-trace-all=" + emptyAll);
        System.out.println("suppressed-max=" + suppressedMax);
    }
}
