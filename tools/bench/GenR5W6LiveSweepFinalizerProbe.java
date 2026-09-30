// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.Reference;

/**
 * gen r5w6/old10 (2026-09-27): the O(live) old-gen collection
 * ({@code CRATONVM_GC_OLD_LIVE_SWEEP}) with a REACHABLE finalizable in old
 * gen. Until this wave any registered finalizer candidate in old gen made
 * every later stop-the-world old-gen collection ineligible for the O(live)
 * path, silently; now a reached one leaves it on, and only an unreached one
 * sends that collection to the walked path (which resurrects it)
 * ({@code docs/internal/gc/gengc-r5w3-oldgen7-proposal-bot-object-base-oracle-for-an-o-live-sweep-DONE-20260928.md}).
 *
 * <p>Allocates one finalizable ({@code Fin}) and {@code n} nodes, tenures
 * them with two {@code System.gc()}, drops all but one node in 20, collects
 * again, and checks the survivors and the finalizable. The finalizable stays
 * reachable to the end ({@code Reference.reachabilityFence}), so no VM runs
 * its {@code finalize()}.
 *
 * <p>stdout, identical on HotSpot and CratonVM (default {@code n = 200000}):
 * <pre>
 *   survivors ok count=10000 checksum=30996970000
 *   finalizable kept ok v=42 finalized=0
 *   PASS
 * </pre>
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W6LiveSweepFinalizerProbe
 *   CRATONVM_GC_OLD_LIVE_SWEEP=1 RUST_LOG=cratonvm::gc=debug cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W6LiveSweepFinalizerProbe
 * </pre>
 * The CratonVM arm's stderr must show one {@code old-gen live sweep (O(live))}
 * line per stop-the-world old-gen collection after the finalizable was
 * tenured, and no {@code old-gen live sweep declined} line. Before gen
 * r5w6/old10 it showed none after the finalizable was tenured.
 */
public final class GenR5W6LiveSweepFinalizerProbe {
    static volatile int finalized;

    static final class Fin {
        final long v;

        Fin(long v) {
            this.v = v;
        }

        @SuppressWarnings({"deprecation", "removal"})
        @Override
        protected void finalize() {
            finalized++;
        }
    }

    static final class Node {
        final long v;

        Node(long v) {
            this.v = v;
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        Fin keep = new Fin(42);
        Node[] nodes = new Node[n];
        for (int i = 0; i < n; i++) {
            nodes[i] = new Node((long) i * 31 + 7);
        }
        System.gc();
        System.gc();
        long expectCount = 0;
        long expectSum = 0;
        for (int i = 0; i < n; i++) {
            if (i % 20 != 0) {
                nodes[i] = null;
            } else {
                expectCount++;
                expectSum += (long) i * 31 + 7;
            }
        }
        System.gc();
        System.gc();
        boolean ok = true;
        long count = 0;
        long sum = 0;
        for (int i = 0; i < n; i++) {
            Node x = nodes[i];
            if (i % 20 == 0) {
                if (x == null || x.v != (long) i * 31 + 7) {
                    ok = false;
                } else {
                    count++;
                    sum += x.v;
                }
            } else if (x != null) {
                ok = false;
            }
        }
        ok &= count == expectCount && sum == expectSum;
        System.out.println((ok ? "survivors ok" : "survivors FAILED") + " count=" + count
                + " checksum=" + sum);
        boolean finOk = keep.v == 42 && finalized == 0;
        System.out.println((finOk ? "finalizable kept ok" : "finalizable FAILED") + " v=" + keep.v
                + " finalized=" + finalized);
        Reference.reachabilityFence(keep);
        boolean pass = ok && finOk;
        System.out.println(pass ? "PASS" : "FAIL");
        if (!pass) {
            System.exit(1);
        }
    }
}
