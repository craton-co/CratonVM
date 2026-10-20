// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.lang.management.MemoryPoolMXBean;
import java.lang.management.MemoryUsage;
import java.util.ArrayList;
import java.util.List;

/**
 * gcj j1/yg ({@code gengc-r5w3-evac7-jit-warm-young-cycles-sweep-in-place-and-fragment-young-RETIRED-UNFIXED-20261018}):
 * JIT-warm young cycles on a NEAR-FULL heap, the shape on which the
 * promotion-OOM guard (old and young both at least 90 % full, conservative
 * JIT roots live) keeps a term-4 cycle from the pinned in-place copy.
 *
 * <ol>
 *   <li>Fill the old generation with long-lived {@code long[1024]} blocks until
 *       the tenured pool (the non-eden, non-survivor heap pool) reads at least
 *       {@code args[0]} percent (default 92) of its max, or {@code args[1]} MiB
 *       (default 100) have been retained, whichever comes first.</li>
 *   <li>Run a compiled loop of {@code args[2]} iterations (default 40 000 000)
 *       that replaces entries of a 65 536-node ring (live, aging) and allocates
 *       a dead {@code byte[48]} per iteration.</li>
 * </ol>
 *
 * <p>stdout (deterministic, HotSpot's): {@code gcj1yg promo-risk done
 * checksum=<c>}, where {@code c} depends only on the loop's arithmetic.
 * stderr: the retained MiB and the tenured pool reading (they differ by
 * collector and are not compared). Read the VM side's {@code [GC]
 * young_term4_offer:} and {@code decision histogram} lines with
 * {@code CRATONVM_GC_STATS=1}.
 */
public class Gcj1YgPromotionRiskProbe {
    static final class Node {
        long v;
        int[] pad = new int[4];

        Node(long v) {
            this.v = v;
        }
    }

    static volatile byte[] sink;

    public static void main(String[] args) {
        int pct = args.length > 0 ? Integer.parseInt(args[0]) : 92;
        long capMib = args.length > 1 ? Long.parseLong(args[1]) : 100L;
        long iters = args.length > 2 ? Long.parseLong(args[2]) : 40_000_000L;

        List<long[]> retained = new ArrayList<>();
        long retainedBytes = 0;
        MemoryPoolMXBean tenured = tenuredPool();
        while (retainedBytes < capMib * 1024L * 1024L) {
            for (int k = 0; k < 256; k++) {
                retained.add(new long[1024]);
                retainedBytes += 8L * 1024L;
            }
            if (tenured != null) {
                MemoryUsage u = tenured.getUsage();
                if (u.getMax() > 0 && u.getUsed() * 100L >= u.getMax() * (long) pct) {
                    break;
                }
            }
        }
        if (tenured != null) {
            MemoryUsage u = tenured.getUsage();
            System.err.println("[gcj1yg-promo] tenured " + tenured.getName() + " used=" + u.getUsed()
                    + " max=" + u.getMax());
        }
        System.err.println("[gcj1yg-promo] retained_mib=" + (retainedBytes >> 20));

        Node[] ring = new Node[65536];
        for (int i = 0; i < ring.length; i++) {
            ring[i] = new Node(i);
        }
        long checksum = 0;
        for (long i = 0; i < iters; i++) {
            int slot = (int) (i * 2654435761L) & (ring.length - 1);
            Node n = new Node(i ^ ring[slot].v);
            n.pad[(int) (i & 3)] = (int) i;
            ring[slot] = n;
            byte[] dead = new byte[48];
            dead[(int) (i & 47)] = (byte) i;
            sink = dead;
            checksum += n.v & 0xff;
        }
        for (Node n : ring) {
            checksum = checksum * 31 + n.v;
        }
        long keep = 0;
        for (long[] b : retained) {
            keep += b.length;
        }
        System.err.println("[gcj1yg-promo] kept_words=" + keep);
        System.out.println("gcj1yg promo-risk done checksum=" + checksum);
    }

    /** The heap pool that is neither eden nor a survivor space (Serial's "Tenured Gen"). */
    static MemoryPoolMXBean tenuredPool() {
        for (MemoryPoolMXBean p : ManagementFactory.getMemoryPoolMXBeans()) {
            String n = p.getName();
            if (p.getType() == java.lang.management.MemoryType.HEAP && !n.contains("Eden")
                    && !n.contains("Survivor")) {
                return p;
            }
        }
        return null;
    }
}
