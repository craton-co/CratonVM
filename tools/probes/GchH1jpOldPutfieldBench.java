// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;

/**
 * gch h1/jp (2026-10-08): the row the two {@code gcf-f1p-*} pages never had --
 * a compiled reference {@code putfield} whose RECEIVER is old, so the
 * generational post barrier has a card to mark. {@code GcfF3pOldStoreBench}
 * has array modes only, and {@code GcfF1pAllocBarrierBench store}'s holders
 * stopped being old (gcg g1b/p: {@code helper=0}).
 *
 * <p>A tenured {@code Holder[]} of tenured {@code Holder}s; each round a
 * {@code System.gc()} and about 48 MiB of young garbage (whose young
 * collections clean the holders' cards), then one young {@code Leaf} stored
 * into every holder's {@code ref} field by the timed method. With
 * {@code CRATONVM_JIT_INLINE_CARD_MARK=0} every one of those stores is the
 * {@code jit_putfield_object} helper ({@code vm/src/jit/helpers.rs}); by
 * default the compiled card check / card store settles most of them inline.
 *
 * <p>STDOUT: one deterministic line, identical on HotSpot 25:
 * <pre>
 *   mode=old-putfield n=N rounds=R checksum=C
 * </pre>
 * STDERR: {@code [bench-timing] ... steady_median_round_us=Y ns_per_op=Z}.
 *
 * <pre>
 *   GchH1jpOldPutfieldBench [log2=14] [rounds=60]
 * </pre>
 */
public final class GchH1jpOldPutfieldBench {
    static final class Leaf {
        final long v;

        Leaf(long v) {
            this.v = v;
        }
    }

    static final class Holder {
        Leaf ref;
        long pad;
    }

    static long sink;
    static Object escape;

    /** The timed store: every {@code holders[i]} is old, every {@code src[i]} young. */
    static void storeAll(Holder[] holders, Leaf[] src) {
        for (int i = 0; i < holders.length; i++) {
            holders[i].ref = src[i];
        }
    }

    /** About 48 MiB of short-lived garbage: several young collections. */
    static long churnYoung(long acc) {
        for (int i = 0; i < 48 * 1024; i++) {
            byte[] junk = new byte[1024];
            escape = junk;
            acc += junk.length;
        }
        escape = null;
        return acc;
    }

    static void tenure() {
        long s = 0;
        for (int w = 0; w < 8; w++) {
            Object[] churn = new Object[1 << 16];
            for (int i = 0; i < churn.length; i++) {
                churn[i] = new long[4];
            }
            s += churn.length;
        }
        for (int g = 0; g < 3; g++) {
            System.gc();
        }
        if (s == 42) {
            System.out.println("unreachable");
        }
    }

    public static void main(String[] args) {
        int log2 = args.length > 0 ? Integer.parseInt(args[0]) : 14;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        int n = 1 << log2;
        Holder[] holders = new Holder[n];
        for (int i = 0; i < n; i++) {
            holders[i] = new Holder();
            holders[i].pad = i;
        }
        tenure();
        long[] nanos = new long[rounds];
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            System.gc();
            sink = churnYoung(sink);
            Leaf[] src = new Leaf[n];
            for (int i = 0; i < n; i++) {
                src[i] = new Leaf(((long) r << 32) + i);
            }
            long t0 = System.nanoTime();
            storeAll(holders, src);
            nanos[r] = System.nanoTime() - t0;
            src = null;
            for (int i = 0; i < n; i += 97) {
                checksum += (holders[i].ref.v & 0xFFFFF) + holders[i].pad;
            }
        }
        System.out.println("mode=old-putfield n=" + n + " rounds=" + rounds + " checksum=" + checksum);
        int skip = rounds / 10;
        long[] steady = Arrays.copyOfRange(nanos, skip, rounds);
        long[] all = nanos.clone();
        Arrays.sort(steady);
        Arrays.sort(all);
        long steadyMed = steady[steady.length / 2];
        long total = 0;
        for (long v : nanos) {
            total += v;
        }
        System.err.println("[bench-timing] mode=old-putfield n=" + n + " rounds=" + rounds
                + " total_ms=" + (total / 1_000_000) + " median_round_us=" + (all[all.length / 2] / 1_000)
                + " steady_median_round_us=" + (steadyMed / 1_000) + " ns_per_op=" + (steadyMed / n));
    }
}
