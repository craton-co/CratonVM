// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.OperatingSystemMXBean;

/**
 * gcj j3/og3 (2026-10-19): a reference-dense, fragmented old generation whose
 * majors COMPACT on the Generational backend, for the old-gen compaction
 * switches' A/B ({@code CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX},
 * {@code CRATONVM_GC_OLD_MARK_LOADER_SNAPSHOT}, the Phase 0 drop).
 *
 * <p>Why {@code Gcj1OgFullGcCompactProbe} never compacted on the j1c rows: its
 * {@code System.gc()} calls sit inside {@code main}'s hot round loop, which the
 * JIT compiles (OSR), so every requested major ran with a live compiled frame.
 * A requested major on the non-moving path compacts
 * ({@code CRATONVM_GC_REQUESTED_MAJOR_COMPACTS}, default ON) only when the pause
 * has no conservative root to pin ({@code OldPinnedCompact::pins_nothing_conservative});
 * with a compiled frame live it reclaims in place and counts
 * {@code oldsz_requested_major_compaction_declines}. Here every hot loop runs
 * in a helper method that has RETURNED before {@code main} (interpreted: it
 * loops {@code rounds} times only) calls {@code System.gc()}.
 *
 * <p>Modes:
 * <ul>
 * <li>{@code req}: {@code rounds} rounds of "replace {@code replacePct} % of the
 * live nodes, relink, {@code System.gc()}". On Generational each is a requested
 * major through the pinned-object compactor ({@code compact_around_pins}).
 * <li>{@code alloc}: no {@code System.gc()} after the build; each round also
 * allocates {@code churnMb} MiB of short-lived arrays, so the replaced nodes
 * tenure and die in the old generation and the occupancy trigger (75 %) runs
 * the majors from the moving young cycle's Phase 5. Those majors take the
 * full-GC SLIDE ({@code compact_with_walked_objects_shared}) only with no
 * compiled frame live ({@code --nojit}) and P-C unable to arm
 * ({@code CRATONVM_GC_MOVING_MAJOR_TRUE_ROOTS=0}), or with a fragmentation
 * request pending.
 * </ul>
 *
 * <p>stdout is identical on HotSpot ({@code -XX:+UseSerialGC}) and CratonVM:
 * <pre>
 *   Gcj3Og3CompactingMajorProbe mode=req live=400000 rounds=12 replacePct=10
 *   ring=400000 checksum=&lt;n&gt;
 *   PROBE-OK
 * </pre>
 * stderr (informational): {@code [og3] loop-ms=<wall> cpu-ms=<process cpu or -1>
 * full-gc-count=<n> full-gc-ms=<bean time>}.
 *
 * <p>Args (all optional): mode live rounds replacePct churnMb seed.
 */
public final class Gcj3Og3CompactingMajorProbe {
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
        String mode = args.length > 0 ? args[0] : "req";
        int live = args.length > 1 ? Integer.parseInt(args[1]) : 400_000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 12;
        int replacePct = args.length > 3 ? Integer.parseInt(args[3]) : 10;
        int churnMb = args.length > 4 ? Integer.parseInt(args[4]) : 32;
        rng = args.length > 5 ? Long.parseLong(args[5]) : 0x9E3779B97F4A7C15L;
        boolean req = !"alloc".equals(mode);
        System.out.println("Gcj3Og3CompactingMajorProbe mode=" + (req ? "req" : "alloc")
                + " live=" + live + " rounds=" + rounds + " replacePct=" + replacePct);

        Node[] nodes = build(live);
        // Tenure the live set and the interleaved garbage (it must die IN
        // the old generation to leave holes there).
        System.gc();
        sink = null;

        long cpu0 = processCpuNanos();
        long t0 = System.nanoTime();
        if (req) {
            System.gc();
        }
        int replace = (int) ((long) live * replacePct / 100);
        // `main` loops `rounds` times only: it stays interpreted, so no
        // compiled frame is live at the System.gc() below.
        for (int r = 0; r < rounds; r++) {
            replaceSome(nodes, replace);
            link(nodes);
            if (req) {
                System.gc();
            } else {
                churn(churnMb);
            }
        }
        long loopMs = (System.nanoTime() - t0) / 1_000_000;
        long cpu1 = processCpuNanos();

        long checksum = checksum(nodes);
        int ringLen = ringLength(nodes);
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
        long cpuMs = (cpu0 < 0 || cpu1 < 0) ? -1 : (cpu1 - cpu0) / 1_000_000;
        System.err.println("[og3] loop-ms=" + loopMs + " cpu-ms=" + cpuMs
                + " full-gc-count=" + count + " full-gc-ms=" + timeMs);
    }

    /** {@code live} nodes interleaved with as many garbage nodes. */
    static Node[] build(int live) {
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
        sink = garbage;
        return nodes;
    }

    static void replaceSome(Node[] nodes, int replace) {
        int live = nodes.length;
        for (int k = 0; k < replace; k++) {
            int i = nextInt(live);
            Node fresh = new Node(nodes[i].v);
            if (nodes[i].side != null) {
                fresh.side = new Object[4];
            }
            nodes[i] = fresh;
        }
    }

    /** Short-lived garbage: drives young cycles (and promotion) in mode alloc. */
    static void churn(int mb) {
        long acc = 0;
        for (int i = 0; i < mb * 16; i++) {
            long[] a = new long[(1 << 16) / 8 - 4];
            a[i & 7] = i;
            acc += a.length;
        }
        sink = acc == 42 ? null : sink;
    }

    /** Ring successor plus a deterministic far edge (an earlier node). */
    static void link(Node[] nodes) {
        int live = nodes.length;
        for (int i = 0; i < live; i++) {
            Node a = nodes[i];
            a.next = nodes[(i + 1) % live];
            a.far = nodes[(int) (((long) i * 2654435761L) % live)];
        }
    }

    static long checksum(Node[] nodes) {
        long checksum = 0;
        int seen = 0;
        int live = nodes.length;
        Node n = nodes[0];
        do {
            checksum += n.v * 31 + (n.far == null ? 7 : n.far.v);
            if (n.side != null) {
                checksum += ((Object[]) n.side).length;
            }
            seen++;
            n = n.next;
        } while (n != nodes[0] && seen <= live);
        return checksum;
    }

    static int ringLength(Node[] nodes) {
        int len = 0;
        Node n = nodes[0];
        do {
            len++;
            n = n.next;
        } while (n != nodes[0] && len <= nodes.length);
        return len;
    }

    /** Process CPU in ns, or -1 where the platform bean does not offer it. */
    static long processCpuNanos() {
        try {
            OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean) {
                return ((com.sun.management.OperatingSystemMXBean) os).getProcessCpuTime();
            }
        } catch (Throwable t) {
            // fall through
        }
        return -1;
    }
}
