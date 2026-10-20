// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gen r4w5/concmark5 (2026-09-24): the retirement probe for
 * {@code docs/internal/gaps/gengc-r4w2-concmark-jit-gate-takeover-window-20260923.md}
 * (status "FIX LANDED, awaiting probe": gen r4w4/concmark4's frozen-thread
 * eager scan at the generational initial mark).
 *
 * <p>The page's race, as a workload. Every thread owns {@code holders} old
 * objects (allocated first and kept, so they are promoted) whose one field
 * points at a {@code Token}. In a hot, JIT-compiled loop each thread
 * <ol>
 *   <li>reads the token of a RANDOM holder of ANOTHER thread (page step 3:
 *       "T2 reads A from O.f"),</li>
 *   <li>stores it into one of its OWN holders (page step 3: "...and stores it
 *       into an already-scanned object P"), and</li>
 *   <li>now and then overwrites one of its own holders with a FRESH token
 *       (page step 4: "T1 completes O.f = B"), which drops the old token.</li>
 * </ol>
 * Tokens become old too (they survive in holders long enough to be promoted),
 * the steady drops make old garbage, and a retained churn ring keeps the old
 * generation growing, so generational concurrent cycles keep opening while
 * compiled reference stores run on every thread — the only setting in which
 * the xt takeover can freeze a thread between a compiled store's SATB gate test
 * and its store.
 *
 * <p>Every token read is validated: {@code magic == id * 0x9E3779B1 ^ 0x5A5A5A5A}.
 * A token the concurrent sweep freed while it was still reachable (the page's
 * use-after-free) reads back a wrong magic, a wrong class, or crashes. So the
 * program prints {@code corrupt=0} on a correct VM and on HotSpot:
 * <pre>
 *   concmark-jit-gate threads=8 holders=4096 iters=4000000 corrupt=0
 * </pre>
 *
 * <p><b>Why this probe is not deterministic, and what is.</b> Whether an
 * initial-mark pause freezes a thread INSIDE the few instructions between a
 * gate test and its store is a matter of timing; no Java program can force it,
 * and no JVM flag parks a compiled store there. So the runtime oracle is an
 * IMPLICATION over the shutdown census, checked on every run, plus
 * {@code corrupt=0}:
 * <pre>
 *   [GC] conc_policy: ... concpol_initial_marks=I concpol_initial_marks_with_takeover=K
 *       concpol_frozen_threads=F concpol_frozen_objects_scanned=S ...
 *   K > 0  implies  S > 0     (the eager scan engaged on every pause that froze someone)
 *   I >= 1                    (a cycle opened at all; otherwise the run is void: raise iters)
 * </pre>
 * A run with {@code K = 0} is INCONCLUSIVE, not a pass: repeat (the orchestrator
 * runs 10; the page retires when at least one run has {@code K > 0}, every such
 * run has {@code S > 0}, and every run prints {@code corrupt=0}). The kill-switch
 * arm ({@code CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN=1}) must print
 * {@code concpol_frozen_objects_scanned=0} — the control that proves the counter
 * is the fix's. The DETERMINISTIC half of the evidence is the unit test that
 * stages the page's exact sequence in both arms,
 * {@code concurrent_mark::tests::a_frozen_threads_unlogged_store_cannot_hide_a_snapshot_value};
 * a VM-level deterministic test would need a JIT stub parked between the gate
 * test and the store by the xt harness ({@code vm/src/jit/xt_root_scan.rs}) —
 * proposed, not written (not this lane's file).
 *
 * <p>Commands (build: {@code javac -d tools/bench tools/bench/GenR4W5ConcMarkJitGateProbe.java}):
 * <pre>
 *   java -XX:+UseG1GC -Xmx256m -cp tools/bench GenR4W5ConcMarkJitGateProbe
 *   for i in $(seq 10); do
 *     CRATONVM_DBG=gc-stats CRATONVM_GC_CONC_START_PERCENT=20 cratonvm --java-home "$JDK" \
 *         -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W5ConcMarkJitGateProbe 2>&1 \
 *       | grep -E 'concmark-jit-gate|conc_policy:'
 *   done
 *   CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN=1 CRATONVM_DBG=gc-stats CRATONVM_GC_CONC_START_PERCENT=20 \
 *       cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m \
 *       -cp tools/bench GenR4W5ConcMarkJitGateProbe 2>&1 | grep -E 'concmark-jit-gate|conc_policy:'
 * </pre>
 * {@code CRATONVM_GC_CONC_START_PERCENT=20} opens cycles early and often, which
 * multiplies the initial-mark pauses the takeover can land in.
 *
 * <p>Usage: {@code GenR4W5ConcMarkJitGateProbe [threads] [holders] [iters] [churnRing]}.
 */
public final class GenR4W5ConcMarkJitGateProbe {
    static final class Token {
        final int id;
        final int magic;
        final long[] body;

        Token(int id) {
            this.id = id;
            this.magic = id * 0x9E3779B1 ^ 0x5A5A5A5A;
            this.body = new long[4];
            this.body[id & 3] = id;
        }

        boolean valid() {
            return magic == (id * 0x9E3779B1 ^ 0x5A5A5A5A) && body != null && body.length == 4
                    && body[id & 3] == id;
        }
    }

    static final class Holder {
        Token f;
    }

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    /** xorshift32: deterministic per thread, allocation-free. */
    static int next(int x) {
        x ^= x << 13;
        x ^= x >>> 17;
        x ^= x << 5;
        return x;
    }

    public static void main(String[] args) throws InterruptedException {
        final int threads = arg(args, 0, 8);
        final int holders = arg(args, 1, 4096);
        final int iters = arg(args, 2, 4_000_000);
        final int churnRing = arg(args, 3, 20_000);
        final Holder[][] all = new Holder[threads][holders];
        for (int t = 0; t < threads; t++) {
            for (int h = 0; h < holders; h++) {
                Holder o = new Holder();
                o.f = new Token(t * holders + h);
                all[t][h] = o;
            }
        }
        final long[] corrupt = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int me = t;
            ts[t] = new Thread(() -> {
                Holder[] mine = all[me];
                Object[] churn = new Object[churnRing];
                int x = 0x1234567 + me * 7919;
                int fresh = threads * holders + me;
                long bad = 0;
                for (int i = 0; i < iters; i++) {
                    x = next(x);
                    int other = (me + 1 + ((x >>> 8) & 0x7fffffff) % Math.max(1, threads - 1)) % threads;
                    Holder o = all[other][(x & 0x7fffffff) % holders];
                    Token a = o.f; // step 3: read another thread's O.f
                    if (a == null || !a.valid()) {
                        bad++;
                    } else {
                        mine[(x >>> 3 & 0x7fffffff) % holders].f = a; // ...into our P
                    }
                    if ((i & 63) == 0) {
                        // step 4: overwrite one of our own holders; the old token
                        // may now be reachable only through some other thread's P.
                        mine[(x >>> 5 & 0x7fffffff) % holders].f = new Token(fresh);
                        fresh += threads;
                    }
                    if ((i & 15) == 0) {
                        // Old-generation growth: live long enough to be promoted.
                        churn[(i >>> 4) % churnRing] = new long[16];
                    }
                }
                for (Holder h : mine) {
                    if (h.f == null || !h.f.valid()) {
                        bad++;
                    }
                }
                corrupt[me] = bad;
            }, "jit-gate-" + t);
            ts[t].start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long total = 0;
        for (long c : corrupt) {
            total += c;
        }
        System.out.println("concmark-jit-gate threads=" + threads + " holders=" + holders
                + " iters=" + iters + " corrupt=" + total);
    }
}
