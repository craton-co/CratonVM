// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.locks.LockSupport;

/**
 * gcd d6/s (2026-09-28): a probe built to give the pinned young copy's
 * TAKE-OVER arm ({@code CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER}) the pause it
 * exists for: helper windows the pass can pin WHOLE.
 *
 * <p>Four worker threads spend their lives in a compiled method that holds
 * young objects in locals across {@code LockSupport.parkNanos}, so at almost
 * every young collection each worker is BLOCKED in the park native under
 * compiled frames: a helper window. Their stacks are shallow and ordinary
 * (a normal Java thread's band, a published shadow window), so the pass should
 * read each one whole -- unlike the VM and JDK daemon windows that made
 * {@code GenR4W4EvacThroughputProbe}'s windows refuse
 * ({@code docs/internal/gc/gcd-d5s-takeover-arm-sees-only-refusing-helper-windows-FIXED-20260928.md}).
 * The main thread churns young garbage with a medium-lived ring, forcing the
 * collections. Every worker checks its objects after every park, and the main
 * thread re-derives its ring at the end, so a relocation under a window that
 * was not honoured is a {@code FAIL}, not a different checksum.
 *
 * <p>HotSpot ({@code java -XX:+UseSerialGC -Xmx256m -cp tools/bench Gcd1PinnedTakeoverYoungProbe})
 * prints
 * <pre>
 *   takeover workers=4 parks=12000 bad=0
 *   churn ring=4096 bad=0 checksum=137496576
 *   PASS
 * </pre>
 * and exits 0; otherwise the last line is {@code FAIL} and the exit code 1.
 *
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1PinnedTakeoverYoungProbe.java
 *   P="--java-home $JDK -XX:+UseGenerationalGC -Xmx256m -cp tools/bench"
 *   CRATONVM_DBG=gc-stats cratonvm $P Gcd1PinnedTakeoverYoungProbe
 *   CRATONVM_GEN_PINNED_YOUNG_COPY=1 CRATONVM_GEN_PINNED_YOUNG_COPY_TAKEOVER=1 \
 *     CRATONVM_DBG=gc-stats CRATONVM_DBG_XT_JIT_ROOT_SCAN=1 cratonvm $P Gcd1PinnedTakeoverYoungProbe
 * </pre>
 * With both flags, {@code [GC] young_pinned_takeover:} should read
 * {@code ptko_taken} above zero and {@code [GC] young_pinned_takeover_declines:}
 * say why every other reached cycle declined; the per-window debug lines give
 * {@code complete=true shadow_ok=true depth=Some(..)} for the workers.
 */
public final class Gcd1PinnedTakeoverYoungProbe {
    static final int WORKERS = 4;
    static final int PARKS = 3_000;
    static final int RING = 4096;

    static final class Obj {
        final int v;
        final int[] pad;

        Obj(int v) {
            this.v = v;
            this.pad = new int[] {v, ~v};
        }

        boolean ok(int want) {
            return v == want && pad[0] == want && pad[1] == ~want;
        }
    }

    static final int[] workerBad = new int[WORKERS];
    static final int[] workerParks = new int[WORKERS];

    /**
     * Compiled after warm-up: three young objects live across the park, then
     * checked. Returns 1 for each bad object.
     */
    static int parkHolding(int seed) {
        final Obj a = new Obj(seed);
        final Obj b = new Obj(seed + 1);
        final Obj c = new Obj(seed + 2);
        LockSupport.parkNanos(200_000L);
        return (a.ok(seed) ? 0 : 1) + (b.ok(seed + 1) ? 0 : 1) + (c.ok(seed + 2) ? 0 : 1);
    }

    static void worker(int id) {
        int bad = 0;
        for (int i = 0; i < PARKS; i++) {
            bad += parkHolding(id * 1_000_000 + i);
        }
        workerBad[id] = bad;
        workerParks[id] = PARKS;
    }

    static int len(int seed) {
        return 16 + (seed & 15);
    }

    static int[] payload(int seed) {
        final int[] p = new int[len(seed)];
        for (int i = 0; i < p.length; i++) {
            p[i] = seed * 31 + i;
        }
        return p;
    }

    public static void main(String[] args) throws Exception {
        final Thread[] ws = new Thread[WORKERS];
        for (int t = 0; t < WORKERS; t++) {
            final int id = t;
            ws[t] = new Thread(() -> worker(id), "takeover-worker-" + t);
            ws[t].start();
        }
        // The churn: a ring of 4096 medium-lived arrays, each replaced after
        // RING allocations, plus short-lived garbage, until every worker is
        // done (and at least a fixed amount, so the ring's content is fixed).
        final int[][] ring = new int[RING][];
        final int[] ringSeed = new int[RING];
        long sink = 0;
        int n = 0;
        boolean alive = true;
        while (alive || n < 4_000_000) {
            final int slot = n & (RING - 1);
            if (n < 4_000_000) {
                ring[slot] = payload(n);
                ringSeed[slot] = n;
                n++;
            }
            final int[] garbage = new int[64];
            garbage[n & 63] = n;
            sink += garbage[n & 63];
            if ((n & 0xffff) == 0 || n >= 4_000_000) {
                alive = false;
                for (Thread w : ws) {
                    alive |= w.isAlive();
                }
                if (n >= 4_000_000 && alive) {
                    Thread.sleep(1);
                }
            }
        }
        for (Thread w : ws) {
            w.join();
        }
        int workerBadSum = 0;
        int parks = 0;
        for (int t = 0; t < WORKERS; t++) {
            workerBadSum += workerBad[t];
            parks += workerParks[t];
        }
        int ringBad = 0;
        long checksum = 0;
        for (int s = 0; s < RING; s++) {
            final int seed = ringSeed[s];
            final int[] p = ring[s];
            if (p == null || p.length != len(seed)) {
                ringBad++;
                continue;
            }
            for (int i = 0; i < p.length; i++) {
                if (p[i] != seed * 31 + i) {
                    ringBad++;
                    break;
                }
            }
            checksum += p[0] & 0xffff;
        }
        if (sink == 42) {
            System.out.println("unreachable");
        }
        System.out.println("takeover workers=" + WORKERS + " parks=" + parks + " bad=" + workerBadSum);
        System.out.println("churn ring=" + RING + " bad=" + ringBad + " checksum=" + checksum);
        final boolean ok = workerBadSum == 0 && ringBad == 0 && parks == WORKERS * PARKS;
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
