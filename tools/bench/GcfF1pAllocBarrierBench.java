// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.Arrays;
import java.util.function.LongUnaryOperator;

/**
 * gcf f1/p (2026-10-01): the Generational allocation and barrier row set, built
 * so an interleaved A/B on Linux can decide a switch. Four modes:
 *
 * <ul>
 *   <li>{@code store}: OLD-to-YOUNG reference stores. A tenured {@code Leaf[]}
 *       (one store per slot, scattered, so every card of the array is touched)
 *       and a tenured {@code Node[]} of holders receive YOUNG {@code Leaf}s
 *       from two allocation-free methods ({@link #storeWide} is an
 *       {@code aastore}, {@link #storeNodes} a reference {@code putfield}).
 *       The arm under test is {@code CRATONVM_JIT_INLINE_CARD_MARK}.</li>
 *   <li>{@code alloc}: small-object allocation ({@code new Leaf} + {@code new
 *       Pair} per iteration), 1..N threads: the inline TLAB {@code new}.</li>
 *   <li>{@code array}: small array allocation ({@code new int[8]} + {@code new
 *       Object[4]} per iteration, a young-receiver {@code aastore} between
 *       them), 1..N threads: the inline {@code newarray} and the young skip.</li>
 *   <li>{@code lambda}: a CAPTURING lambda evaluated per iteration, 1..N
 *       threads: the proxy allocation of
 *       {@code common-w9c-proposal-lambda-proxy-tlab-allocation}.</li>
 * </ul>
 *
 * <h2>Why the predecessor bench could not decide cards4</h2>
 *
 * {@code GenR4W4CardBarrierBenchProbe} allocates its young {@code Leaf}s INSIDE
 * the timed store loop, in {@code main}. So (1) young collections run inside
 * the timed region, and under its {@code CRATONVM_GC_VERIFY_RSET=1} rows each
 * one walks the whole old generation; (2) the loop lives in a method with a
 * {@code new}, which the optimizing tier does not compile by default
 * ({@code c2_alloc_upgrade} is opt-in), so neither tier's inline card check was
 * isolated. Its ~84 ms rounds are ~1.2 us per store, two orders of magnitude
 * above any barrier. Here the timed methods contain no allocation and no call,
 * the young values are allocated before the clock starts, and the timing rows
 * set no verification flag.
 *
 * <h2>Output</h2>
 *
 * STDOUT is one deterministic line per run, identical on HotSpot 25:
 * <pre>
 *   mode=store slots=65536 holders=4096 rounds=300 reps=4 mismatches=0 checksum=C
 *   mode=alloc threads=T per_round=P rounds=R checksum=C
 * </pre>
 * STDERR carries the A/B number, one line:
 * <pre>
 *   [bench-timing] mode=M ... median_round_us=X steady_median_round_us=Y ns_per_op=Z
 * </pre>
 * Compare {@code steady_median_round_us} (median over all rounds but the first
 * tenth, and over all threads) across interleaved runs.
 *
 * <h2>Usage</h2>
 * <pre>
 *   GcfF1pAllocBarrierBench store  [slotsLog2=16] [holders=4096] [rounds=300] [reps=4] [churnDepth=10]
 *   GcfF1pAllocBarrierBench alloc  [threads=1] [perRound=200000] [rounds=200]
 *   GcfF1pAllocBarrierBench array  [threads=1] [perRound=200000] [rounds=200]
 *   GcfF1pAllocBarrierBench lambda [threads=1] [perRound=200000] [rounds=200]
 * </pre>
 * The rows and their pass bounds are in
 * {@code docs/internal/gc-design-perf-round-20261001/f1-p-report.md}.
 */
public final class GcfF1pAllocBarrierBench {
    static final class Leaf {
        final long v;

        Leaf(long v) {
            this.v = v;
        }
    }

    static final class Node {
        Object ref;
        final int id;

        Node(int id) {
            this.id = id;
        }
    }

    static final class Pair {
        final Object a;
        final long b;

        Pair(Object a, long b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class Tree {
        Tree a, b;
        int v;

        Tree(int v) {
            this.v = v;
        }
    }

    // ------------------------------------------------------------------
    // store mode
    // ------------------------------------------------------------------

    /**
     * The timed {@code aastore}: {@code wide} is old, every {@code src[i]} is
     * young. The slot walk is a permutation (7919 is odd), so each card of
     * {@code wide} is first stored into while clean and then while dirty, as a
     * real remembered set sees it. No allocation, no call.
     */
    static void storeWide(Leaf[] wide, Leaf[] src, int mask) {
        for (int i = 0; i < src.length; i++) {
            wide[(i * 7919) & mask] = src[i];
        }
    }

    /** The timed reference {@code putfield}: every holder is old. */
    static void storeNodes(Node[] nodes, Leaf[] src) {
        for (int j = 0; j < nodes.length; j++) {
            nodes[j].ref = src[j];
        }
    }

    static long slotKey(int round, int slot) {
        return ((long) round << 32) ^ (slot * 0x9E3779B1L);
    }

    /** Young values for one round, allocated before the clock starts. */
    static Leaf[] fill(int n, int mask, int round) {
        Leaf[] src = new Leaf[n];
        for (int i = 0; i < n; i++) {
            src[i] = new Leaf(slotKey(round, (i * 7919) & mask));
        }
        return src;
    }

    static Tree build(int depth, int v) {
        Tree t = new Tree(v);
        if (depth > 0) {
            t.a = build(depth - 1, v * 2);
            t.b = build(depth - 1, v * 2 + 1);
        }
        return t;
    }

    static long sum(Tree t) {
        return t == null ? 0 : t.v + sum(t.a) + sum(t.b);
    }

    static void runStore(String[] args) {
        int log2 = intArg(args, 1, 16);
        int holders = intArg(args, 2, 4096);
        int rounds = intArg(args, 3, 300);
        int reps = intArg(args, 4, 4);
        int churnDepth = intArg(args, 5, 10);
        int n = 1 << log2;
        int mask = n - 1;
        if (holders > n) {
            holders = n;
        }

        Leaf[] wide = new Leaf[n];
        Node[] nodes = new Node[holders];
        for (int j = 0; j < holders; j++) {
            nodes[j] = new Node(j);
        }
        // Tenure the holders: several young collections and explicit full ones.
        long sink = 0;
        for (int w = 0; w < 8; w++) {
            sink += sum(build(churnDepth + 3, w));
        }
        for (int g = 0; g < 3; g++) {
            System.gc();
        }

        long[] roundNanos = new long[rounds];
        long checksum = 0;
        long mismatches = 0;
        for (int r = 0; r < rounds; r++) {
            Leaf[] src = fill(n, mask, r);
            long t0 = System.nanoTime();
            for (int k = 0; k < reps; k++) {
                storeWide(wide, src, mask);
                storeNodes(nodes, src);
            }
            roundNanos[r] = System.nanoTime() - t0;

            // Young collections happen here, while the Leafs are reachable only
            // through old-to-young edges (and `src`).
            sink += sum(build(churnDepth, r));

            for (int k = 0; k < n; k++) {
                Leaf o = wide[k];
                long want = slotKey(r, k);
                if (o == null || o.v != want) {
                    if (mismatches < 10) {
                        System.out.println("MISMATCH wide slot=" + k + " round=" + r);
                    }
                    mismatches++;
                } else {
                    checksum += want & 0xFFFF;
                }
            }
            for (int j = 0; j < holders; j++) {
                Object o = nodes[j].ref;
                long want = slotKey(r, (j * 7919) & mask);
                if (!(o instanceof Leaf) || ((Leaf) o).v != want) {
                    if (mismatches < 10) {
                        System.out.println("MISMATCH node=" + j + " round=" + r);
                    }
                    mismatches++;
                } else {
                    checksum += (want >>> 7) & 0xFFFF;
                }
            }
        }
        if (sink == 42) {
            System.out.println("unreachable");
        }
        System.out.println("mode=store slots=" + n + " holders=" + holders + " rounds=" + rounds
                + " reps=" + reps + " mismatches=" + mismatches + " checksum=" + checksum);
        long ops = (long) reps * (n + holders);
        timing("store", "slots=" + n + " holders=" + holders + " reps=" + reps,
                new long[][] {roundNanos}, ops);
    }

    // ------------------------------------------------------------------
    // allocation modes
    // ------------------------------------------------------------------

    /** Small objects. The ring is young, so its stores need no card. */
    static long allocLoop(int count, long seed, Object[] ring) {
        long sum = 0;
        for (int i = 0; i < count; i++) {
            Leaf l = new Leaf(seed + i);
            Pair p = new Pair(l, i);
            ring[i & 255] = p;
            sum += ((Leaf) p.a).v ^ p.b;
        }
        return sum;
    }

    /** Small arrays: a primitive one inside a reference one. */
    static long arrayLoop(int count, long seed, Object[] ring) {
        long sum = 0;
        for (int i = 0; i < count; i++) {
            int[] a = new int[8];
            a[i & 7] = i;
            Object[] o = new Object[4];
            o[i & 3] = a;
            ring[i & 255] = o;
            sum += a[i & 7] + o.length + seed;
        }
        return sum;
    }

    /** A capturing lambda per iteration: one proxy allocation each. */
    static long lambdaLoop(int count, long seed, Object[] ring) {
        long sum = 0;
        for (int i = 0; i < count; i++) {
            final long k = seed + i;
            LongUnaryOperator f = x -> x + k;
            ring[i & 255] = f;
            sum += f.applyAsLong(i);
        }
        return sum;
    }

    static long oneRound(String mode, int count, long seed) {
        Object[] ring = new Object[256];
        switch (mode) {
            case "alloc":
                return allocLoop(count, seed, ring);
            case "array":
                return arrayLoop(count, seed, ring);
            default:
                return lambdaLoop(count, seed, ring);
        }
    }

    static void runAlloc(String mode, String[] args) throws InterruptedException {
        int threads = intArg(args, 1, 1);
        int perRound = intArg(args, 2, 200_000);
        int rounds = intArg(args, 3, 200);
        long[][] nanos = new long[threads][rounds];
        long[] sums = new long[threads];
        Thread[] ts = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long s = 0;
                for (int r = 0; r < rounds; r++) {
                    long t0 = System.nanoTime();
                    s += oneRound(mode, perRound, (long) id * 1_000_003L + r);
                    nanos[id][r] = System.nanoTime() - t0;
                }
                sums[id] = s;
            }, "gcf-f1p-" + t);
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long checksum = 0;
        for (long s : sums) {
            checksum += s;
        }
        System.out.println("mode=" + mode + " threads=" + threads + " per_round=" + perRound
                + " rounds=" + rounds + " checksum=" + checksum);
        timing(mode, "threads=" + threads + " per_round=" + perRound, nanos, perRound);
    }

    // ------------------------------------------------------------------

    static int intArg(String[] args, int i, int dflt) {
        return args.length > i ? Integer.parseInt(args[i]) : dflt;
    }

    /**
     * One stderr line: the median round over every thread's rounds, and over
     * every thread's rounds but its first tenth, in microseconds, and the
     * steady median per operation in nanoseconds.
     */
    static void timing(String mode, String shape, long[][] perThread, long opsPerRound) {
        int rounds = perThread[0].length;
        int skip = rounds / 10;
        long[] all = new long[perThread.length * rounds];
        long[] steady = new long[perThread.length * (rounds - skip)];
        int a = 0;
        int s = 0;
        long total = 0;
        for (long[] row : perThread) {
            for (int r = 0; r < rounds; r++) {
                all[a++] = row[r];
                total += row[r];
                if (r >= skip) {
                    steady[s++] = row[r];
                }
            }
        }
        long med = median(all);
        long steadyMed = median(steady);
        long nsPerOp = opsPerRound == 0 ? 0 : steadyMed / opsPerRound;
        System.err.println("[bench-timing] mode=" + mode + " " + shape + " rounds=" + rounds
                + " total_ms=" + (total / 1_000_000) + " median_round_us=" + (med / 1_000)
                + " steady_median_round_us=" + (steadyMed / 1_000) + " ns_per_op=" + nsPerOp);
    }

    static long median(long[] v) {
        if (v.length == 0) {
            return 0;
        }
        long[] c = v.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2;
    }

    public static void main(String[] args) throws InterruptedException {
        String mode = args.length > 0 ? args[0] : "store";
        switch (mode) {
            case "store":
                runStore(args);
                break;
            case "alloc":
            case "array":
            case "lambda":
                runAlloc(mode, args);
                break;
            default:
                System.out.println("usage: GcfF1pAllocBarrierBench store|alloc|array|lambda [args]");
        }
    }
}
