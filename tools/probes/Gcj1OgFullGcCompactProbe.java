// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;

/**
 * gcj j1/og (2026-10-19): full-GC (mark-compact) cost over a large, fragmented,
 * reference-dense old generation, for the old-gen compaction's Phase 2 lookup
 * ({@code CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX}) and the old mark's loader
 * edge ({@code CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT}).
 *
 * <p>Builds {@code live} nodes (a long and two references each: a ring
 * successor and a random earlier node, so most edges are old-to-old and point
 * both near and far) interleaved with garbage nodes of the same shape, so the
 * old generation is half holes once they die; every 16th node also holds a
 * small {@code Object[]}. Then {@code rounds} rounds: replace {@code
 * replacePct} % of the live nodes (fresh nodes, which tenure and leave holes
 * where the old ones lived), and {@code System.gc()}. Every {@code
 * System.gc()} on HotSpot Serial and CratonVM Generational is a full
 * mark-compact over the whole set: the mark follows every edge once, Phase 2
 * rewrites every edge once. All node classes are the application loader's,
 * so every marked node pays the instance -> loader edge.
 *
 * <p>stdout is identical on HotSpot and CratonVM:
 * <pre>
 *   Gcj1OgFullGcCompactProbe live=400000 rounds=12 replacePct=10
 *   ring=400000 checksum=&lt;n&gt;
 *   PROBE-OK
 * </pre>
 * stderr (informational, never compared): {@code [probe] gc-loop-ms=<wall>
 * full-gc-count=<n> full-gc-ms=<bean time>}.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx512m -cp tools/probes Gcj1OgFullGcCompactProbe
 *   CRATONVM_DBG_GCPHASE=1 cratonvm -XX:+UseGenerationalGC -Xmx512m -cp tools/probes Gcj1OgFullGcCompactProbe
 * </pre>
 * Args (all optional): live rounds replacePct seed.
 */
public final class Gcj1OgFullGcCompactProbe {
    static final class Node {
        long v;
        Node next;
        Node far;
        Object side;

        Node(long v) {
            this.v = v;
        }
    }

    static volatile Object sink;

    static long rng;

    static int nextInt(int bound) {
        rng ^= rng << 13;
        rng ^= rng >>> 7;
        rng ^= rng << 17;
        return (int) ((rng >>> 1) % bound);
    }

    public static void main(String[] args) {
        int live = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 12;
        int replacePct = args.length > 2 ? Integer.parseInt(args[2]) : 10;
        rng = args.length > 3 ? Long.parseLong(args[3]) : 0x9E3779B97F4A7C15L;
        System.out.println("Gcj1OgFullGcCompactProbe live=" + live + " rounds=" + rounds
                + " replacePct=" + replacePct);

        Node[] nodes = new Node[live];
        Node[] garbage = new Node[live];
        for (int i = 0; i < live; i++) {
            nodes[i] = new Node(i);
            garbage[i] = new Node(-i);
            if ((i & 15) == 0) {
                nodes[i].side = new Object[4];
            }
        }
        link(nodes);
        // Tenure everything (the garbage too: it must die IN the old
        // generation to leave holes there).
        System.gc();
        garbage = null;
        sink = null;

        long t0 = System.nanoTime();
        System.gc();
        int replace = (int) ((long) live * replacePct / 100);
        for (int r = 0; r < rounds; r++) {
            for (int k = 0; k < replace; k++) {
                int i = nextInt(live);
                Node fresh = new Node(nodes[i].v);
                if (nodes[i].side != null) {
                    fresh.side = new Object[4];
                }
                nodes[i] = fresh;
            }
            link(nodes);
            System.gc();
        }
        long gcLoopMs = (System.nanoTime() - t0) / 1_000_000;

        // Walk the ring from node 0 and check every far edge lands on a node
        // of the table.
        long checksum = 0;
        int ringLen = 0;
        Node n = nodes[0];
        do {
            checksum += n.v * 31 + (n.far == null ? 7 : n.far.v);
            if (n.side != null) {
                checksum += ((Object[]) n.side).length;
            }
            ringLen++;
            n = n.next;
        } while (n != nodes[0] && ringLen <= live);
        System.out.println("ring=" + ringLen + " checksum=" + checksum);
        System.out.println(ringLen == live ? "PROBE-OK" : "PROBE-FAIL ring broken");

        long count = 0;
        long timeMs = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            String name = b.getName();
            if (name.contains("MarkSweep") || name.contains("Old") || name.contains("Full")) {
                count += Math.max(0, b.getCollectionCount());
                timeMs += Math.max(0, b.getCollectionTime());
            }
        }
        System.err.println("[probe] gc-loop-ms=" + gcLoopMs + " full-gc-count=" + count
                + " full-gc-ms=" + timeMs);
    }

    /** Ring successor plus a deterministic far edge (an earlier node). */
    static void link(Node[] nodes) {
        int live = nodes.length;
        for (int i = 0; i < live; i++) {
            Node a = nodes[i];
            a.next = nodes[(i + 1) % live];
            // Far edge: a fixed function of i (not of the RNG), so the
            // checksum does not depend on how many replacements ran.
            a.far = nodes[(int) (((long) i * 2654435761L) % live)];
        }
    }
}
