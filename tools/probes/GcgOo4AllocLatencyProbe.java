// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcg g4/oo (2026-10-02),
 * {@code gengc-r5w2-conc6-proposal-the-failing-allocation-should-not-wait-for-the-cycle}:
 * the allocation-latency tail at steady old-generation promotion.
 *
 * <p>One or more threads each keep a ring of {@code ring} cells, so every cell
 * is promoted and then dies in the old generation (the
 * {@code GenR4W4SteadyPromotionProbe} shape), and every iteration also
 * allocates a young-only {@code byte[pad]}. Each iteration is timed with
 * {@code System.nanoTime()}. With no service thread, a Generational
 * concurrent cycle that the allocation-failure door finds due runs INLINE in
 * the allocation that failed, so that one iteration takes about one whole
 * concurrent cycle; with the cycle owed to the next safepoint poll
 * ({@code CRATONVM_GEN_CONC_OWED_POLL=1} plus the JIT poll hunk), the worst
 * iteration should fall to about one young pause.
 *
 * <p>Stdout is a pure function of the arguments (HotSpot prints the same):
 * <pre>
 *   alloc-latency threads=1 ring=150000 iters=4000000 checksum=&lt;n&gt;
 * </pre>
 * Stderr (diagnostics, not compared): one line per thread and a summary,
 * <pre>
 *   [probe] alloc-latency max_us=&lt;worst iteration&gt; over_1ms=&lt;n&gt; over_10ms=&lt;n&gt; iters=&lt;n&gt;
 * </pre>
 * The first {@code warm} iterations of each thread (default a quarter) are not
 * timed, so the interpreter and compilation do not enter the tail.
 *
 * <p>Usage: {@code GcgOo4AllocLatencyProbe [threads] [ring] [iters] [pad]}.
 */
public final class GcgOo4AllocLatencyProbe {
    static final class Cell {
        final long v;
        final byte[] pad;

        Cell(long v, int pad) {
            this.v = v;
            this.pad = new byte[pad];
        }
    }

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    public static void main(String[] args) throws InterruptedException {
        final int threads = arg(args, 0, 1);
        final int ring = arg(args, 1, 150_000);
        final int iters = arg(args, 2, 4_000_000);
        final int pad = arg(args, 3, 64);
        final int warm = iters / 4;
        final long[] sums = new long[threads];
        final long[] maxNs = new long[threads];
        final long[] over1 = new long[threads];
        final long[] over10 = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                Cell[] r = new Cell[ring];
                long s = 0;
                long worst = 0;
                long o1 = 0;
                long o10 = 0;
                long prev = System.nanoTime();
                for (int i = 0; i < iters; i++) {
                    int k = i % ring;
                    Cell old = r[k];
                    if (old != null) {
                        s += old.v + old.pad.length;
                    }
                    r[k] = new Cell(i * 31L + id, pad);
                    byte[] tmp = new byte[pad];
                    tmp[i % pad] = (byte) i;
                    s += tmp[i % pad];
                    long now = System.nanoTime();
                    if (i >= warm) {
                        long d = now - prev;
                        if (d > worst) {
                            worst = d;
                        }
                        if (d > 1_000_000L) {
                            o1++;
                        }
                        if (d > 10_000_000L) {
                            o10++;
                        }
                    }
                    prev = now;
                }
                for (Cell c : r) {
                    if (c != null) {
                        s += c.v;
                    }
                }
                sums[id] = s;
                maxNs[id] = worst;
                over1[id] = o1;
                over10[id] = o10;
            }, "latency-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        long worst = 0;
        long o1 = 0;
        long o10 = 0;
        for (int t = 0; t < threads; t++) {
            total += sums[t];
            worst = Math.max(worst, maxNs[t]);
            o1 += over1[t];
            o10 += over10[t];
            System.err.println("[probe] thread=" + t + " max_us=" + (maxNs[t] / 1000)
                    + " over_1ms=" + over1[t] + " over_10ms=" + over10[t]);
        }
        System.err.println("[probe] alloc-latency max_us=" + (worst / 1000) + " over_1ms=" + o1
                + " over_10ms=" + o10 + " iters=" + ((long) threads * (iters - warm)));
        System.out.println("alloc-latency threads=" + threads + " ring=" + ring
                + " iters=" + iters + " checksum=" + total);
    }
}
