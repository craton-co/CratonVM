// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Interpreter allocation scaling probe for G1's {@code needs_gc()} path.
 *
 * <p>Run with {@code --nojit -XX:+UseG1GC} and compare equal-work runs at one
 * and eight threads. The total number of allocations is fixed across the
 * sweep, so the wall-time change measures scaling rather than extra work. A
 * worker retains no allocated object after its iteration, but folds every
 * value into a checksum so allocation elimination or a lost worker cannot
 * produce a plausible success line.
 *
 * <p>Usage: {@code G1NeedsGcContentionProbe [threads] [totalIterations]}.
 */
public final class G1NeedsGcContentionProbe {
    private static final int DEFAULT_TOTAL_ITERATIONS = 4_000_000;

    private static final class Node {
        final int tag;
        Node(int tag) { this.tag = tag; }
    }

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        int total = args.length > 1 ? Integer.parseInt(args[1]) : DEFAULT_TOTAL_ITERATIONS;
        if (threads < 1 || total < threads) {
            throw new IllegalArgumentException("need threads >= 1 and totalIterations >= threads");
        }

        final long[] sums = new long[threads];
        final int perThread = total / threads;
        final int remainder = total % threads;
        final Thread[] workers = new Thread[threads];
        final java.util.concurrent.CountDownLatch ready =
                new java.util.concurrent.CountDownLatch(threads);
        final java.util.concurrent.CountDownLatch start =
                new java.util.concurrent.CountDownLatch(1);

        for (int worker = 0; worker < threads; worker++) {
            final int id = worker;
            final int count = perThread + (id < remainder ? 1 : 0);
            workers[worker] = new Thread(() -> {
                ready.countDown();
                try {
                    start.await();
                } catch (InterruptedException e) {
                    throw new AssertionError(e);
                }
                long sum = 0;
                for (int i = 0; i < count; i++) {
                    Node node = new Node(i ^ (id * 0x9e37));
                    sum += node.tag;
                }
                sums[id] = sum;
            }, "g1-needs-gc-" + worker);
            workers[worker].start();
        }

        ready.await();
        long began = System.nanoTime();
        start.countDown();
        for (Thread worker : workers) {
            worker.join();
        }
        long elapsedMs = (System.nanoTime() - began) / 1_000_000L;

        long checksum = 0;
        for (long sum : sums) {
            checksum += sum;
        }
        System.out.println("G1NeedsGcContentionProbe threads=" + threads
                + " totalIterations=" + total
                + " wallMs=" + elapsedMs
                + " checksum=" + checksum
                + " PROBE-OK");
    }
}
