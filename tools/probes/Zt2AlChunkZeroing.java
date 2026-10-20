// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Craton Software Company

/**
 * zt t2/al: the varlen allocation shape over RECYCLED chunks, for the
 * TLAB-chunk zeroing A/B (`CRATONVM_ZGC_CHUNK_ZERO_DESCENDING`) and its
 * soundness check.
 *
 * <p>Every round allocates one million {@code long[len]} arrays, dirties one
 * element of each, and keeps the last 4096 alive so survivors are scattered
 * across the swept chunks and the refills recycle free-list blocks instead of
 * carving pristine memory.
 *
 * <p>The zeroing check (a recycled chunk carries the stale bytes this probe
 * wrote into its predecessors) reads a SAMPLE: every 1024th array, published
 * through a {@code volatile} field and read back before it is checked
 * (zt t6/rv3, page {@code zgc-zt3x2-*} item 1). A check right after
 * {@code new long[len]} with no store in between is foldable: an optimizing
 * compiler (HotSpot C2, and this VM's IR tier) answers a load from a fresh,
 * unwritten array with a constant 0, so {@code bad} was dead code on the
 * compiled part of the run. The fence keeps the sampled loads real, and the
 * sampling keeps the fence off the timed loop. There is no probe-level
 * positive control (no switch skips the zeroing); the unit test
 * {@code zt2al_descending_chunk_zeroing_reads_zero_and_counts} is it.
 *
 * <p>Usage: {@code Zt2AlChunkZeroing [rounds=20] [len=32]}. Stdout is one
 * deterministic line, the same on HotSpot:
 * {@code zt2al chunk-zeroing rounds=R len=L made=M sampled=S bad=0 ok=true}.
 * Time it with {@code /usr/bin/time -f '%U %S'} (CPU), not wall.
 */
public class Zt2AlChunkZeroing {
    /** The publication the sampled check reads back through. */
    static volatile long[] sample;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        int len = args.length > 1 ? Integer.parseInt(args[1]) : 32;
        if (len < 1) {
            len = 1;
        }
        long[][] keep = new long[4096][];
        long bad = 0;
        long made = 0;
        long sampled = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < 1_000_000; i++) {
                long[] a = new long[len];
                if ((i & 1023) == 0) {
                    sample = a;
                    long[] b = sample;
                    for (int j = 0; j < len; j++) {
                        if (b[j] != 0) {
                            bad++;
                        }
                    }
                    sampled++;
                }
                a[i % len] = i | 1L;
                keep[i & 4095] = a;
                made++;
            }
        }
        long kept = 0;
        for (long[] a : keep) {
            if (a != null) {
                kept++;
            }
        }
        System.out.println("zt2al chunk-zeroing rounds=" + rounds + " len=" + len + " made=" + made
                + " sampled=" + sampled + " bad=" + bad
                + " ok=" + (bad == 0 && kept == keep.length));
    }
}
