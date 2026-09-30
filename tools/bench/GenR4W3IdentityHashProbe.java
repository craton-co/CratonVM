// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.IdentityHashMap;

/**
 * gen r4w3/hunter (2026-09-23): identity hash codes must survive every object
 * move the generational collector makes — a young-to-survivor copy, a copy by
 * the parallel evacuator, promotion into old gen, and the old-gen compaction of
 * a full {@code System.gc()} — including for objects that were also used as
 * monitors (lock inflation is where a displaced header most often loses its
 * hash bits) and for arrays.
 *
 * <p>Each round hashes a fresh cohort of young objects, keeps them reachable
 * from an old array, churns enough garbage to force several young collections,
 * then re-checks EVERY cohort hashed so far, plus an {@code IdentityHashMap}
 * keyed by all of them (a stale hash makes {@code get} miss). Cohorts hashed in
 * early rounds are re-checked after they have been copied and promoted.
 *
 * <p>Deterministic output on HotSpot (any collector):
 * <pre>
 *   PASS rounds=40 objects=40000 mismatches=0 mapMisses=0
 * </pre>
 * Commands:
 * <pre>
 *   java -Xmx256m -cp tools/bench GenR4W3IdentityHashProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W3IdentityHashProbe
 *   CRATONVM_DBG=gc-stress=250000 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W3IdentityHashProbe
 * </pre>
 * Optional args: {@code rounds perRound churnMiB}. Exit status 1 on FAIL.
 */
public final class GenR4W3IdentityHashProbe {
    static final class Box {
        final int id;
        long pad0, pad1;

        Box(int id) {
            this.id = id;
        }
    }

    static volatile Object sink;

    public static void main(String[] args) {
        final int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        final int perRound = args.length > 1 ? Integer.parseInt(args[1]) : 1000;
        final int churnMiB = args.length > 2 ? Integer.parseInt(args[2]) : 64;
        final int total = rounds * perRound;

        final Object[] objs = new Object[total];
        final int[] hashes = new int[total];
        final IdentityHashMap<Object, Integer> map = new IdentityHashMap<>();
        long mismatches = 0;
        long mapMisses = 0;

        for (int r = 0; r < rounds; r++) {
            final int base = r * perRound;
            for (int i = 0; i < perRound; i++) {
                final int k = base + i;
                final Object o;
                switch (k % 4) {
                    case 0: o = new Box(k); break;
                    case 1: o = new int[1 + (k % 7)]; break;
                    case 2: o = new Object[] {Integer.valueOf(k)}; break;
                    default: o = new Box(-k); break;
                }
                objs[k] = o;
                hashes[k] = System.identityHashCode(o);
                if ((k & 3) == 3) {
                    // hash first, THEN use as a monitor; every 64th also
                    // waits briefly so an inflating implementation inflates
                    synchronized (o) {
                        if ((k & 63) == 3) {
                            try {
                                o.wait(1);
                            } catch (InterruptedException e) {
                                Thread.currentThread().interrupt();
                            }
                        }
                    }
                }
                map.put(o, k);
            }
            churn(churnMiB);
            if (r % 10 == 9) {
                System.gc(); // full collection: old-gen moves too
            }
            for (int k = 0; k < base + perRound; k++) {
                final Object o = objs[k];
                if (System.identityHashCode(o) != hashes[k] || o.hashCode() != hashes[k]) {
                    if (mismatches < 5) {
                        System.out.println("MISMATCH round=" + r + " k=" + k + " kind=" + (k % 4)
                                + " was=" + hashes[k] + " now=" + System.identityHashCode(o));
                    }
                    mismatches++;
                }
                final Integer v = map.get(o);
                if (v == null || v.intValue() != k) {
                    mapMisses++;
                }
            }
        }
        final String verdict = (mismatches == 0 && mapMisses == 0) ? "PASS" : "FAIL";
        System.out.println(verdict + " rounds=" + rounds + " objects=" + total
                + " mismatches=" + mismatches + " mapMisses=" + mapMisses);
        if (!"PASS".equals(verdict)) {
            System.exit(1);
        }
    }

    /** Allocate {@code mib} MiB of short-lived garbage in 4 KiB chunks. */
    static void churn(int mib) {
        for (int i = 0; i < mib * 256; i++) {
            sink = new byte[4096 - 16];
        }
        sink = null;
    }
}
