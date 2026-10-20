// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;
import java.util.HashMap;

/**
 * gcf f3/p (2026-10-01): the shape the inline generational card check
 * ({@code CRATONVM_JIT_INLINE_CARD_MARK}) and the card-store form
 * ({@code CRATONVM_JIT_CARD_STORE_BARRIER}) are supposed to pay on: reference
 * stores of YOUNG values into a big OLD table whose card is already dirty --
 * sequential indices, 64 (128 under narrow oops) stores per 512-byte card, so
 * only the first store of a card per young cycle finds it clean. This is
 * {@code HashMap.putVal}'s {@code tab[i] = newNode(..)} and the shape
 * {@code docs/known-issues/gc/jit-r15w5-gcpage-proposals.md} GP-2 names (its
 * probe, {@code R15GcpageYoungPremium}, is not in the tree; these modes are
 * its {@code old-store} / {@code alloc-old-store} / {@code hashmap-pair} rows,
 * rebuilt).
 *
 * <ul>
 *   <li>{@code old-store}: young {@code Leaf}s, allocated before the clock,
 *       stored into a tenured {@code Leaf[]} in index order. No allocation and
 *       no call in the timed method.</li>
 *   <li>{@code alloc-old-store}: {@code table[i] = new Leaf(..)} in index order
 *       (allocation plus an old-to-young store per iteration).</li>
 *   <li>{@code hashmap-pair}: a presized, tenured {@code HashMap<Integer,
 *       Integer>} cleared and refilled from pre-boxed keys each round (every
 *       {@code put} allocates a young node and stores it into the old table).</li>
 *   <li>{@code old-store-clean} (gcg g4/jm): the CLEAN-card twin of
 *       {@code old-store}. The tenured table has {@code 64 * n} slots and the
 *       timed method stores {@code n} young {@code Leaf}s at every 64th index,
 *       one store per 512-byte card (two under narrow oops), after a
 *       {@code System.gc()} and about 48 MiB of young garbage, whose young
 *       collections clean the table's cards (a full collection alone leaves
 *       them dirty; gcg g5/jm, from vg4's engagement row). So nearly every timed
 *       store is the first of its card: the case the inline card check cannot
 *       settle without the card-store form ({@code CRATONVM_JIT_CARD_STORE_BARRIER},
 *       default ON since gcg g6/jm).</li>
 * </ul>
 *
 * STDOUT: one deterministic line, identical on HotSpot 25:
 * <pre>
 *   mode=M n=N rounds=R checksum=C
 * </pre>
 * STDERR: {@code [bench-timing] mode=M ... steady_median_round_us=Y ns_per_op=Z}
 * (the A/B number; compare medians across interleaved runs).
 *
 * <pre>
 *   GcfF3pOldStoreBench old-store       [log2=20] [rounds=60]
 *   GcfF3pOldStoreBench alloc-old-store [log2=20] [rounds=60]
 *   GcfF3pOldStoreBench hashmap-pair    [log2=18] [rounds=60]
 *   GcfF3pOldStoreBench old-store-clean [log2=14] [rounds=60]
 * </pre>
 * Rows and pass bounds: {@code docs/internal/gc-design-perf-round-20261001/f3-p-report.md}.
 */
public final class GcfF3pOldStoreBench {
    static final class Leaf {
        final long v;

        Leaf(long v) {
            this.v = v;
        }
    }

    /** The timed store: {@code table} is old, every {@code src[i]} young. */
    static void storeSeq(Leaf[] table, Leaf[] src) {
        for (int i = 0; i < src.length; i++) {
            table[i] = src[i];
        }
    }

    /** The timed clean-card store: one store per 64 slots of the old table. */
    static void storeStride64(Leaf[] table, Leaf[] src) {
        for (int i = 0; i < src.length; i++) {
            table[i << 6] = src[i];
        }
    }

    static long sink;
    static Object escape;

    /**
     * About 48 MiB of short-lived garbage: several young collections. Each
     * array escapes to a static (the next one overwrites it), so no
     * allocation can be scalar-replaced away.
     */
    static long churnYoung(long acc) {
        for (int i = 0; i < 48 * 1024; i++) {
            byte[] junk = new byte[1024];
            escape = junk;
            acc += junk.length;
        }
        escape = null;
        return acc;
    }

    /** The timed allocate-and-store: one young {@code Leaf} per old slot. */
    static void allocStoreSeq(Leaf[] table, long base) {
        for (int i = 0; i < table.length; i++) {
            table[i] = new Leaf(base + i);
        }
    }

    /** The timed refill of an old-tabled map. */
    static void refill(HashMap<Integer, Integer> map, Integer[] keys) {
        map.clear();
        for (int i = 0; i < keys.length; i++) {
            map.put(keys[i], keys[i]);
        }
    }

    static void tenure() {
        long sink = 0;
        for (int w = 0; w < 8; w++) {
            Object[] churn = new Object[1 << 16];
            for (int i = 0; i < churn.length; i++) {
                churn[i] = new long[4];
            }
            sink += churn.length;
        }
        for (int g = 0; g < 3; g++) {
            System.gc();
        }
        if (sink == 42) {
            System.out.println("unreachable");
        }
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "old-store";
        int log2 = args.length > 1 ? Integer.parseInt(args[1])
                : (mode.equals("hashmap-pair") ? 18 : mode.equals("old-store-clean") ? 14 : 20);
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 60;
        int n = 1 << log2;
        long[] nanos = new long[rounds];
        long checksum = 0;
        switch (mode) {
            case "old-store": {
                Leaf[] table = new Leaf[n];
                tenure();
                for (int r = 0; r < rounds; r++) {
                    Leaf[] src = new Leaf[n];
                    for (int i = 0; i < n; i++) {
                        src[i] = new Leaf(((long) r << 32) + i);
                    }
                    long t0 = System.nanoTime();
                    storeSeq(table, src);
                    nanos[r] = System.nanoTime() - t0;
                    src = null;
                    for (int i = 0; i < n; i += 97) {
                        checksum += table[i].v & 0xFFFFF;
                    }
                }
                break;
            }
            case "old-store-clean": {
                Leaf[] table = new Leaf[n << 6];
                tenure();
                for (int r = 0; r < rounds; r++) {
                    // The table and last round's Leafs old, then young
                    // collections, whose card scan finds no young reference
                    // under the table's cards and cleans them (gcg g5/jm:
                    // vg4's `jm4_aa_eng_on` showed a full collection alone
                    // leaves them dirty -- ir_clean was one round's worth),
                    // then young values.
                    System.gc();
                    sink = churnYoung(sink);
                    Leaf[] src = new Leaf[n];
                    for (int i = 0; i < n; i++) {
                        src[i] = new Leaf(((long) r << 32) + i);
                    }
                    long t0 = System.nanoTime();
                    storeStride64(table, src);
                    nanos[r] = System.nanoTime() - t0;
                    src = null;
                    for (int i = 0; i < n; i += 97) {
                        checksum += table[i << 6].v & 0xFFFFF;
                    }
                }
                break;
            }
            case "alloc-old-store": {
                Leaf[] table = new Leaf[n];
                tenure();
                for (int r = 0; r < rounds; r++) {
                    long t0 = System.nanoTime();
                    allocStoreSeq(table, (long) r << 32);
                    nanos[r] = System.nanoTime() - t0;
                    for (int i = 0; i < n; i += 97) {
                        checksum += table[i].v & 0xFFFFF;
                    }
                }
                break;
            }
            case "hashmap-pair": {
                Integer[] keys = new Integer[n];
                for (int i = 0; i < n; i++) {
                    keys[i] = Integer.valueOf(i * 31);
                }
                HashMap<Integer, Integer> map = new HashMap<>(n * 2);
                refill(map, keys);
                tenure();
                for (int r = 0; r < rounds; r++) {
                    long t0 = System.nanoTime();
                    refill(map, keys);
                    nanos[r] = System.nanoTime() - t0;
                    for (int i = 0; i < n; i += 97) {
                        checksum += map.get(keys[i]);
                    }
                    checksum += map.size();
                }
                break;
            }
            default:
                System.out.println("usage: GcfF3pOldStoreBench old-store|old-store-clean|alloc-old-store|hashmap-pair [log2] [rounds]");
                return;
        }
        System.out.println("mode=" + mode + " n=" + n + " rounds=" + rounds + " checksum=" + checksum);
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
        System.err.println("[bench-timing] mode=" + mode + " n=" + n + " rounds=" + rounds
                + " total_ms=" + (total / 1_000_000) + " median_round_us=" + (all[all.length / 2] / 1_000)
                + " steady_median_round_us=" + (steadyMed / 1_000) + " ns_per_op=" + (steadyMed / n));
    }
}
