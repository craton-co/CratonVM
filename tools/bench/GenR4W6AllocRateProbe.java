// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.concurrent.locks.LockSupport;

/**
 * gen r4w6/tlab6 (2026-09-24): small-object allocation rate, single-threaded,
 * 4-threaded, and 4-threaded with a park every {@code parkEvery} iterations
 * (every park retires the thread's TLAB — the shape
 * {@code CRATONVM_GEN_TLAB_TAIL_SINK} changes).
 *
 * <p>Each iteration of a worker allocates one {@code int[2]} and one
 * {@code Node} holding it, and stores the node in a 1024-slot ring, so both
 * objects escape and most die young. The node it replaces is read back first
 * and adds {@code 3a + b + arr[0]} to the sum, where {@code a} is the index it
 * was allocated at, {@code b} the worker's seed and {@code arr[0] = a & 0xFFFF};
 * the ring's last 1024 nodes are added at the end. Every node is counted once,
 * so a worker with seed {@code s} sums
 * {@code 3 n(n-1)/2 + n s + sum_{i<n} (i & 0xFFFF)} whatever the collector did,
 * as long as no reachable field was lost or moved wrongly.
 *
 * <p>Deterministic lines (defaults: n = 20,000,000 per worker, 4 workers,
 * 3 reps, park every 8192 iterations):
 * <pre>
 * GenR4W6AllocRateProbe n=20000000 threads=4 reps=3 parkEvery=8192
 * single threads=1 checksum=600655028867840 expected=600655028867840 ok
 * multi threads=4 checksum=2402620235471360 expected=2402620235471360 ok
 * parked threads=4 checksum=2402620235471360 expected=2402620235471360 ok
 * verdict=ok
 * </pre>
 * Lines starting with {@code timing} are wall-clock rates (iterations per
 * millisecond, one per phase per rep) and are NOT part of the verdict; compare
 * medians across interleaved runs, and filter them out ({@code grep -v '^timing'})
 * before comparing output against HotSpot.
 * <pre>
 *   java -cp tools/bench GenR4W6AllocRateProbe
 *   cratonvm -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench GenR4W6AllocRateProbe
 *   CRATONVM_GEN_TLAB_TAIL_SINK=1 cratonvm -XX:+UseGenerationalGC -Xmx256m --verbose:gc -cp tools/bench GenR4W6AllocRateProbe
 * </pre>
 * Arguments: iterations per worker (default 20,000,000), workers for the two
 * multi-threaded phases (default 4), reps (default 3), park interval
 * (default 8192; 0 disables the park in the third phase).
 */
public final class GenR4W6AllocRateProbe {
    static final class Node {
        final long a;
        final int b;
        final int[] arr;

        Node(long a, int b, int[] arr) {
            this.a = a;
            this.b = b;
            this.arr = arr;
        }
    }

    static long run(long n, int seed, int parkEvery) {
        Node[] ring = new Node[1024];
        long sum = 0;
        for (long i = 0; i < n; i++) {
            int k = (int) (i & 1023);
            Node old = ring[k];
            if (old != null) {
                sum += old.a * 3 + old.b + old.arr[0];
            }
            int[] arr = new int[2];
            arr[0] = (int) (i & 0xFFFF);
            arr[1] = seed;
            ring[k] = new Node(i, seed, arr);
            if (parkEvery > 0 && i % parkEvery == parkEvery - 1) {
                LockSupport.parkNanos(1_000L);
            }
        }
        for (Node x : ring) {
            if (x != null) {
                sum += x.a * 3 + x.b + x.arr[0];
            }
        }
        return sum;
    }

    /** The closed form of {@link #run} summed over {@code threads} workers seeded 1..threads. */
    static long expected(long n, int threads) {
        long tri = n * (n - 1) / 2;
        long q = n / 65536;
        long r = n % 65536;
        long low = q * 2147450880L + r * (r - 1) / 2;
        long seedSum = (long) threads * (threads + 1) / 2;
        return threads * (3 * tri + low) + n * seedSum;
    }

    static long runThreads(final long n, int threads, final int parkEvery) throws InterruptedException {
        final long[] sums = new long[threads];
        Thread[] workers = new Thread[threads];
        for (int t = 0; t < threads; t++) {
            final int idx = t;
            workers[t] = new Thread(() -> sums[idx] = run(n, idx + 1, parkEvery), "alloc-" + t);
        }
        for (Thread w : workers) {
            w.start();
        }
        for (Thread w : workers) {
            w.join();
        }
        long total = 0;
        for (long s : sums) {
            total += s;
        }
        return total;
    }

    public static void main(String[] args) throws InterruptedException {
        long n = args.length > 0 ? Long.parseLong(args[0]) : 20_000_000L;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        int reps = args.length > 2 ? Integer.parseInt(args[2]) : 3;
        int parkEvery = args.length > 3 ? Integer.parseInt(args[3]) : 8192;
        System.out.println("GenR4W6AllocRateProbe n=" + n + " threads=" + threads
                + " reps=" + reps + " parkEvery=" + parkEvery);

        String[] phases = {"single", "multi", "parked"};
        int[] phaseThreads = {1, threads, threads};
        long[] first = new long[phases.length];
        boolean ok = true;
        for (int rep = 0; rep < reps; rep++) {
            for (int p = 0; p < phases.length; p++) {
                long t0 = System.nanoTime();
                long sum = p == 0
                        ? run(n, 1, 0)
                        : runThreads(n, phaseThreads[p], p == 2 ? parkEvery : 0);
                long ms = Math.max(1L, (System.nanoTime() - t0) / 1_000_000L);
                long exp = expected(n, phaseThreads[p]);
                if (rep == 0) {
                    first[p] = sum;
                    System.out.println(phases[p] + " threads=" + phaseThreads[p] + " checksum=" + sum
                            + " expected=" + exp + (sum == exp ? " ok" : " MISMATCH"));
                }
                if (sum != exp || sum != first[p]) {
                    ok = false;
                    if (rep != 0) {
                        System.out.println(phases[p] + " rep=" + rep + " checksum=" + sum + " MISMATCH");
                    }
                }
                System.out.println("timing (not compared): rep=" + rep + " phase=" + phases[p]
                        + " ops_per_ms=" + (n * phaseThreads[p] / ms));
            }
        }
        System.out.println("verdict=" + (ok ? "ok" : "FAIL"));
    }
}
