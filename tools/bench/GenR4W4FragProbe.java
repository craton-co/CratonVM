// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;
import java.util.List;

/**
 * gen r4w4/oldgen4 (2026-09-24): a contiguous large array must be allocatable
 * when the old generation has the room in total but not in one piece.
 *
 * <p>Geometry at {@code -Xmx256m} (CratonVM's default split): two 64 MiB young
 * semi-spaces, a 128 MiB old generation, humongous threshold 32 MiB. Forty
 * 2 MiB arrays, each followed by a small survivor, are made old (80 MiB live,
 * a ~48 MiB tail); every other large array is dropped and collected, leaving
 * twenty 2 MiB holes (~88 MiB free, largest block ~48 MiB). A 72 MiB array fits
 * no hole, not the tail, and not a 64 MiB young semi-space — only a compacted
 * old generation (live ~40 MiB). HotSpot Serial's full GC is a mark-compact
 * (and its old generation is 2/3 of the heap), so it always fits.
 *
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR4W4FragProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR4W4FragProbe
 * </pre>
 * HotSpot prints, and CratonVM must print:
 * <pre>
 *   fragmented-contiguous ok
 *   PASS
 * </pre>
 * On CratonVM the refused 72 MiB request arms a compaction request
 * ({@code CRATONVM_GC_OLD_OOM_COMPACT}, default on) that the next moving-path
 * old-gen collection honours. Negative control:
 * {@code CRATONVM_GC_OLD_OOM_COMPACT=0} → {@code fragmented-contiguous FAILED:
 * OutOfMemoryError} and {@code FAIL} (the pre-wave-4 behaviour). If the
 * collection that follows the refusal runs on the NON-moving path (a compiled
 * frame's conservative roots), it cannot compact and the error stands — try
 * {@code --nojit} to tell the two apart.
 */
public final class GenR4W4FragProbe {
    static final int MIB = 1 << 20;
    static final int PAIRS = 40;
    static List<byte[]> large = new ArrayList<>();
    static List<long[]> small = new ArrayList<>();

    static void build() {
        for (int i = 0; i < PAIRS; i++) {
            byte[] b = new byte[2 * MIB];
            b[0] = (byte) i;
            b[b.length - 1] = (byte) (i + 1);
            large.add(b);
            long[] s = new long[64];
            s[0] = i;
            small.add(s);
        }
    }

    /** Drops every other large array; own frame, so no stale local roots. */
    static void punchHoles() {
        for (int i = 0; i < PAIRS; i += 2) {
            large.set(i, null);
        }
    }

    public static void main(String[] args) {
        build();
        for (int i = 0; i < 4; i++) {
            System.gc();
        }
        punchHoles();
        for (int i = 0; i < 2; i++) {
            System.gc();
        }
        boolean ok;
        try {
            byte[] big = new byte[72 * MIB];
            big[0] = 1;
            big[big.length - 1] = 2;
            ok = true;
            for (int i = 1; i < PAIRS; i += 2) {
                byte[] b = large.get(i);
                ok &= b[0] == (byte) i && b[b.length - 1] == (byte) (i + 1);
            }
            for (int i = 0; i < PAIRS; i++) {
                ok &= small.get(i)[0] == i;
            }
            System.out.println(ok ? "fragmented-contiguous ok"
                    : "fragmented-contiguous FAILED: a survivor was corrupted");
        } catch (OutOfMemoryError e) {
            ok = false;
            System.out.println("fragmented-contiguous FAILED: OutOfMemoryError");
        }
        System.out.println(ok ? "PASS" : "FAIL");
        if (!ok) {
            System.exit(1);
        }
    }
}
