// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w4/concmark4 (2026-09-24): BURSTS of promotion into the old
 * generation — the case the concurrent-first policy's fallback exists for.
 *
 * <p>Each phase, every thread retains a batch of {@code batch} nodes for
 * {@code hold} rounds of young churn (long enough to be promoted together),
 * then drops it and starts the next. A burst promotes far faster than the
 * steady state, so a concurrent cycle that started at the adaptive threshold
 * may not finish before the generation reaches the 90 % defer ceiling (or an
 * old-gen allocation fails): the STW old-gen collection then runs through the
 * open cycle, and the start policy widens its predicted per-cycle growth so
 * the NEXT cycle starts earlier.
 *
 * <p>Oracle ({@code CRATONVM_DBG=gc-stats}; no HotSpot equivalent):
 * <ul>
 *   <li>the run completes (no {@code OutOfMemoryError}, no hang) and prints
 *       the same line as HotSpot:
 *       <pre>  burst-promotion threads=4 phases=12 batch=100000 checksum=&lt;same on every VM&gt;</pre></li>
 *   <li>{@code [GC] conc_policy: ... concpol_cycles_completed=N} with
 *       {@code N >= 1};</li>
 *   <li>if {@code concpol_stw_preempted=P} is non-zero, then
 *       {@code concpol_predicted_growth} is at least one eighth of
 *       {@code concpol_old_capacity} and {@code concpol_start_pct} is below
 *       45 — the widening happened; P should be small against the number of
 *       phases (the policy learns after the first bursts).</li>
 *   <li>legacy arm ({@code CRATONVM_GC_NO_CONCURRENT_FIRST=1}):
 *       {@code concpol_stw_deferred=0} and {@code concpol_stw_preempted=0}
 *       (the legacy policy never defers), same program output.</li>
 * </ul>
 * Commands (build: {@code javac -d tools/bench tools/bench/GenR4W4BurstPromotionProbe.java}):
 * <pre>
 *   java -XX:+UseG1GC -Xmx256m -cp tools/bench GenR4W4BurstPromotionProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m \
 *       -cp tools/bench GenR4W4BurstPromotionProbe
 *   CRATONVM_GC_NO_CONCURRENT_FIRST=1 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4BurstPromotionProbe
 *   CRATONVM_GC_CONC_START_PERCENT=60 CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" \
 *       -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4BurstPromotionProbe
 * </pre>
 * The last arm pins the threshold (no adaptation): {@code concpol_start_pct=60}
 * on the summary line whatever the bursts did.
 *
 * <p>Usage: {@code GenR4W4BurstPromotionProbe [threads] [phases] [batch] [hold]}.
 */
public final class GenR4W4BurstPromotionProbe {
    static final class Node {
        final long v;
        final long[] payload;

        Node(long v) {
            this.v = v;
            this.payload = new long[6];
            this.payload[(int) (v & 5)] = v;
        }
    }

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    public static void main(String[] args) throws InterruptedException {
        final int threads = arg(args, 0, 4);
        final int phases = arg(args, 1, 12);
        final int batch = arg(args, 2, 100_000);
        final int hold = arg(args, 3, 400_000);
        final long[] sums = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long s = 0;
                for (int p = 0; p < phases; p++) {
                    List<Node> retained = new ArrayList<>(batch);
                    for (int i = 0; i < batch; i++) {
                        retained.add(new Node((long) p * batch + i + id));
                    }
                    // Young churn while the batch is held: it is promoted.
                    for (int i = 0; i < hold; i++) {
                        Node tmp = new Node(i);
                        s += tmp.payload[(int) (tmp.v & 5)] & 1;
                    }
                    for (Node n : retained) {
                        s += n.v + n.payload[(int) (n.v & 5)];
                    }
                    // `retained` dies here: a whole batch of dead old objects.
                }
                sums[id] = s;
            }, "burst-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        for (long s : sums) {
            total += s;
        }
        System.out.println("burst-promotion threads=" + threads + " phases=" + phases
                + " batch=" + batch + " checksum=" + total);
    }
}
