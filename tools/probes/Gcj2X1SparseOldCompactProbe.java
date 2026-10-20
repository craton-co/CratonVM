// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;

/**
 * gcj j2/x1 (2026-10-19): a full-GC compaction of a SPARSE old generation --
 * a few survivors at its base and one near its top, everything between them
 * dead. The old-gen compaction's forward index
 * ({@code CRATONVM_GC_OLD_COMPACT_FORWARD_INDEX}, gcj j1/og) sized its block
 * directory by the span up to the LAST survivor (4 bytes per 512 bytes: 8 MiB
 * per GiB), not by the survivor count; gcj j2/x1 falls back to the hash map
 * for such a table. Soundness is the stdout (every survivor intact after the
 * slide); the engagement and the cost are on stderr.
 *
 * <p>Steps: tenure {@code bottom} (System.gc tenures everything on
 * Generational and HotSpot Serial); tenure {@code mb} MiB of garbage arrays
 * held live; tenure {@code top} above them; drop the garbage and run
 * {@code rounds} System.gc() (the first slides {@code top} down over the dead
 * span; later ones are dense).
 *
 * <p>stdout (HotSpot {@code -XX:+UseSerialGC -Xmx1g}, by construction):
 * <pre>
 *   Gcj2X1SparseOldCompactProbe mb=600 rounds=3
 *   bottom=4096 top=4096 sum=&lt;n&gt;
 *   PROBE-OK
 * </pre>
 * stderr: {@code [x1-sparse] first-gc-ms=<wall> rest-gc-ms=<wall>}.
 *
 * <p>Args (optional): {@code mb rounds}.
 */
public final class Gcj2X1SparseOldCompactProbe {
    static final class Cell {
        final long v;
        Cell next;

        Cell(long v) {
            this.v = v;
        }
    }

    static Cell chain(int n, long seed) {
        Cell head = null;
        for (int i = 0; i < n; i++) {
            Cell c = new Cell(seed + i);
            c.next = head;
            head = c;
        }
        return head;
    }

    static long check(Cell c, long seed, int n) {
        long sum = 0;
        int i = n - 1;
        int seen = 0;
        for (; c != null; c = c.next, i--, seen++) {
            if (c.v != seed + i) {
                return -1;
            }
            sum += c.v;
        }
        return seen == n ? sum : -1;
    }

    public static void main(String[] args) {
        int mb = args.length > 0 ? Integer.parseInt(args[0]) : 600;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 3;
        System.out.println("Gcj2X1SparseOldCompactProbe mb=" + mb + " rounds=" + rounds);
        final int n = 4096;
        Cell bottom = chain(n, 1_000_000L);
        System.gc();
        ArrayList<long[]> garbage = new ArrayList<>(mb);
        for (int i = 0; i < mb; i++) {
            garbage.add(new long[(1 << 20) / 8 - 4]);
        }
        System.gc();
        Cell top = chain(n, 9_000_000L);
        System.gc();
        garbage = null;
        long t0 = System.nanoTime();
        System.gc();
        long t1 = System.nanoTime();
        for (int r = 1; r < rounds; r++) {
            System.gc();
        }
        long t2 = System.nanoTime();
        System.err.println("[x1-sparse] first-gc-ms=" + (t1 - t0) / 1_000_000
                + " rest-gc-ms=" + (t2 - t1) / 1_000_000);
        long b = check(bottom, 1_000_000L, n);
        long t = check(top, 9_000_000L, n);
        System.out.println("bottom=" + (b < 0 ? "BROKEN" : Integer.toString(n))
                + " top=" + (t < 0 ? "BROKEN" : Integer.toString(n)) + " sum=" + (b + t));
        System.out.println(b >= 0 && t >= 0 ? "PROBE-OK" : "PROBE-FAIL");
    }
}
