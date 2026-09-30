// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w4/concmark4 (2026-09-24): STEADY promotion into the old generation,
 * from several mutator threads — the workload the concurrent-first old-gen
 * policy is for.
 *
 * <p>Each thread keeps a ring of {@code ring} cells. A cell lives for exactly
 * {@code ring} of its thread's allocations: long enough to survive several
 * young collections and be PROMOTED, short enough to die in the old
 * generation. So the old generation gains dead-but-promoted cells at a steady
 * rate over a stable live set (the rings), and something has to reclaim them
 * over and over. Every iteration also allocates one young-only temporary.
 *
 * <p>What the policy predicts (read from the {@code CRATONVM_DBG=gc-stats}
 * shutdown summary — HotSpot has no equivalent counters):
 * <ul>
 *   <li><b>concurrent-first (default)</b>: concurrent cycles start at the
 *       initiating occupancy (45 % of the old generation until one cycle is
 *       measured, then {@code 75 % - (5/4 G + C/32)}), finish before the 75 %
 *       STW floor, and the STW old-gen collection almost never runs:
 *       {@code [GC] generational: ... major=M} with {@code M} ~ 0 (0 or 1),
 *       {@code [GC] conc_policy: concpol_policy=concurrent-first ...
 *       concpol_cycles_completed=N} with {@code N >= 1}, and
 *       {@code concpol_stw_preempted} at most 1 (one pre-emption is allowed:
 *       the first cycle starts at the static 45 % before anything is
 *       measured; after it the threshold adapts).</li>
 *   <li><b>legacy</b> ({@code CRATONVM_GC_NO_CONCURRENT_FIRST=1}): both
 *       collectors share the 75 % trigger, so the STW collection inside the
 *       young pause runs first: {@code major=M} with {@code M >= 1}, and
 *       {@code concpol_policy=legacy}.</li>
 * </ul>
 * Program output is a pure function of the arguments (the checksum does not
 * depend on scheduling) and must match HotSpot's byte for byte:
 * <pre>
 *   steady-promotion threads=4 ring=150000 iters=3000000 checksum=&lt;same on every VM&gt;
 * </pre>
 * Commands (build: {@code javac -d tools/bench tools/bench/GenR4W4SteadyPromotionProbe.java}):
 * <pre>
 *   java -XX:+UseG1GC -Xmx512m -Xlog:gc -cp tools/bench GenR4W4SteadyPromotionProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m \
 *       -cp tools/bench GenR4W4SteadyPromotionProbe
 *   CRATONVM_GC_NO_CONCURRENT_FIRST=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx512m -cp tools/bench GenR4W4SteadyPromotionProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx512m -Xlog:gc \
 *       -cp tools/bench GenR4W4SteadyPromotionProbe | grep 'Concurrent Mark Cycle'
 * </pre>
 * The last command must print at least one
 * {@code GC(c1) Concurrent Mark Cycle <a>M-><b>M(<cap>M) <t>ms} line with
 * {@code b < a} (old-generation occupancy before and after the cycle); the
 * G1 command prints HotSpot's own {@code Concurrent Mark Cycle} lines for the
 * shape. {@code --verbose:gc} prints the same cycles as
 * {@code [GC] concurrent-cycle: concyc_n=... concyc_next_start_pct=...}.
 *
 * <p>Usage: {@code GenR4W4SteadyPromotionProbe [threads] [ring] [iters] [pad]}.
 */
public final class GenR4W4SteadyPromotionProbe {
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
        final int threads = arg(args, 0, 4);
        final int ring = arg(args, 1, 150_000);
        final int iters = arg(args, 2, 3_000_000);
        final int pad = arg(args, 3, 64);
        final long[] sums = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                Cell[] r = new Cell[ring];
                long s = 0;
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
                }
                for (Cell c : r) {
                    if (c != null) {
                        s += c.v;
                    }
                }
                sums[id] = s;
            }, "promoter-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        for (long s : sums) {
            total += s;
        }
        System.out.println("steady-promotion threads=" + threads + " ring=" + ring
                + " iters=" + iters + " checksum=" + total);
    }
}
