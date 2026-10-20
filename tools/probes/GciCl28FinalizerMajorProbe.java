// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.atomic.AtomicInteger;

/**
 * gci i28/cl (2026-10-15): finalizer-heavy requested majors over a large live
 * old set, for {@code CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL}
 * ({@code gcg-g3e-proposal-post-resurrection-closure-scans-only-what-the-round-marked-20261001}).
 *
 * <p>Builds a live linked list of {@code live} nodes (each with a side array),
 * tenures it by allocation churn, then runs {@code rounds} rounds: allocate
 * {@code fins} finalizables, each holding a private chain of {@code depth}
 * nodes whose last node points back into the live list; tenure them by churn;
 * drop them; {@code System.gc()}. On the Generational backend each such
 * {@code System.gc()} is a true-root major whose finalizer round is NOT idle
 * (dead old candidates with subgraphs), so the post-resurrection closure runs
 * after it -- the closure the switch narrows to what the round marked.
 *
 * <p>Every {@code finalize()} re-reads its chain and compares the sum with the
 * one it was built with: a chain freed under its resurrected owner reads as a
 * mismatch (or crashes). stdout is identical on HotSpot and CratonVM:
 * <pre>
 *   GciCl28FinalizerMajorProbe rounds=40 fins=2000 depth=4 live=300000
 *   finalized=80000/80000 mismatch=0
 *   live count=300000 checksum=44999850000
 *   PROBE-OK
 * </pre>
 * stderr: {@code [probe] rounds-ms=<t>} (the timed loop, informational).
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx512m -Xmn32m -cp tools/probes GciCl28FinalizerMajorProbe
 *   CRATONVM_DBG_GCPHASE=1 CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL=1 cratonvm -XX:+UseGenerationalGC \
 *       -Xmx512m -Xmn32m --nojit -cp tools/probes GciCl28FinalizerMajorProbe
 * </pre>
 * Args (all optional): rounds fins depth live churnMb.
 */
public final class GciCl28FinalizerMajorProbe {
    static final AtomicInteger FINALIZED = new AtomicInteger();
    static final AtomicInteger MISMATCH = new AtomicInteger();
    static volatile Object sink;

    static final class Node {
        final long v;
        Node next;
        Object side;

        Node(long v, Node next) {
            this.v = v;
            this.next = next;
        }
    }

    static final class Fin {
        final Node chain;
        final long expected;

        Fin(Node chain, long expected) {
            this.chain = chain;
            this.expected = expected;
        }

        @SuppressWarnings("removal")
        @Override
        protected void finalize() {
            long s = 0;
            for (Node n = chain; n != null; n = n.next) {
                s += n.v;
            }
            if (s != expected) {
                MISMATCH.incrementAndGet();
            }
            FINALIZED.incrementAndGet();
        }
    }

    static void churn(int mb) {
        for (int i = 0; i < mb * 64; i++) {
            sink = new byte[16 * 1024];
        }
        sink = null;
    }

    static int arg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    @SuppressWarnings("removal")
    public static void main(String[] args) throws Exception {
        int rounds = arg(args, 0, 40);
        int fins = arg(args, 1, 2000);
        int depth = arg(args, 2, 4);
        int live = arg(args, 3, 300000);
        int churnMb = arg(args, 4, 48);
        System.out.println("GciCl28FinalizerMajorProbe rounds=" + rounds + " fins=" + fins
                + " depth=" + depth + " live=" + live);

        Node head = null;
        for (int i = 0; i < live; i++) {
            head = new Node(i, head);
            head.side = new int[] {i};
        }
        Node[] holder = {head};
        head = null;
        churn(churnMb * 2);

        long t0 = System.nanoTime();
        for (int r = 0; r < rounds; r++) {
            Fin[] batch = new Fin[fins];
            for (int f = 0; f < fins; f++) {
                long base = (long) r * 1_000_000L + (long) f * depth + 1;
                Node chain = null;
                long sum = 0;
                for (int d = 0; d < depth; d++) {
                    chain = new Node(base + d, chain);
                    sum += base + d;
                    if (d == 0) {
                        // The chain's tail points back into the live set.
                        chain.side = holder[0];
                    }
                }
                batch[f] = new Fin(chain, sum);
            }
            churn(churnMb);
            batch = null;
            System.gc();
            System.runFinalization();
        }
        long roundsMs = (System.nanoTime() - t0) / 1_000_000L;

        int want = rounds * fins;
        for (int i = 0; i < 500 && FINALIZED.get() < want; i++) {
            System.gc();
            System.runFinalization();
            Thread.sleep(20);
        }
        int got = FINALIZED.get();
        int bad = MISMATCH.get();
        System.out.println("finalized=" + got + "/" + want + " mismatch=" + bad);

        long count = 0;
        long checksum = 0;
        for (Node n = holder[0]; n != null; n = n.next) {
            count++;
            checksum += n.v;
            if (!(n.side instanceof int[]) || ((int[]) n.side)[0] != n.v) {
                bad++;
            }
        }
        System.out.println("live count=" + count + " checksum=" + checksum);
        System.out.println(got == want && bad == 0 && count == live ? "PROBE-OK" : "PROBE-FAIL");
        System.err.println("[probe] rounds-ms=" + roundsMs);
    }
}
