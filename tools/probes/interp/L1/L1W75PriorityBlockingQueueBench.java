// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L1 -- bench probe for the
// `JdkPriorityBlockingQueue` row (`CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE`,
// default OFF): with it, over a real JDK `--compatible` runs the JDK 25
// `PriorityBlockingQueue` bodies (a `ReentrantLock`, a binary heap sifted with
// `compareTo`) instead of the nine `native-collections` stand-ins (a native
// door, the queue's monitor, and a SORTED array: binary search plus an O(n)
// shift on every insert and every poll).
//
// A/B, one binary, `--compatible` over a real JDK:
//   cratonvm --compatible L1W75PriorityBlockingQueueBench                                              (row off, default)
//   CRATONVM_COMPAT_JDK_PRIORITY_BLOCKING_QUEUE=1 cratonvm --compatible L1W75PriorityBlockingQueueBench (row on)
// and `cratonvm L1W75PriorityBlockingQueueBench` (jdk-only) as the reference the
// row-on arm should equal. Interleave the arms and take medians of the per-run
// minimums (in-JVM timings swing ~3x on a noisy host). Add `--nojit` for the
// interpreter-only picture.
//
// Rows (stdout, min ns/op over REPS=5 rounds):
//   size            size() on a 16-element queue;
//   peek            peek() on a 16-element queue;
//   offer-poll-max  offer(<new maximum>); poll() on a 16-element queue, ns per pair
//                   (stand-in: no shift on insert, a 16-element shift on poll);
//   offer-poll-min  offer(<new minimum>); poll() on a 16-element queue, ns per pair
//                   (stand-in: a 16-element shift each way; JDK: sift to the root and back);
//   drain256        offer 256 values in a scrambled order, then poll all 256, ns per
//                   element (offer + poll); the stand-in's shifts are O(n) here.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum size=3200000 peek=20000000 offer-poll-max=1254975000 offer-poll-min=2475000 drain256=32640 drain256-out-of-order=0
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.PriorityBlockingQueue;

public class L1W75PriorityBlockingQueueBench {
    static final int REPS = 5;
    static final int N = 200_000;
    static final int PAIRS = 50_000;
    static final int DRAINS = 100;

    static PriorityBlockingQueue<Integer> base() {
        PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
        // 100..115, offered high to low.
        for (int v = 115; v >= 100; v--) {
            q.offer(v);
        }
        return q;
    }

    static long size(PriorityBlockingQueue<Integer> q) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += q.size();
        }
        return sum;
    }

    static long peek(PriorityBlockingQueue<Integer> q) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += q.peek();
        }
        return sum;
    }

    /** Pair i offers 116 + i, the new maximum; poll answers 100 + i. */
    static long offerPollMax() {
        PriorityBlockingQueue<Integer> q = base();
        long sum = 0;
        for (int i = 0; i < PAIRS; i++) {
            q.offer(116 + i);
            sum += q.poll();
        }
        return sum;
    }

    /** Pair i offers i % 100, below every element of the base; poll answers it. */
    static long offerPollMin() {
        PriorityBlockingQueue<Integer> q = base();
        long sum = 0;
        for (int i = 0; i < PAIRS; i++) {
            q.offer(i % 100);
            sum += q.poll();
        }
        return sum;
    }

    static long[] drain256() {
        PriorityBlockingQueue<Integer> q = new PriorityBlockingQueue<>();
        long sum = 0;
        long outOfOrder = 0;
        for (int r = 0; r < DRAINS; r++) {
            for (int k = 0; k < 256; k++) {
                // 97 is odd, so k * 97 mod 256 is a permutation of 0..255.
                q.offer((k * 97) & 255);
            }
            sum = 0;
            int last = -1;
            for (int k = 0; k < 256; k++) {
                int v = q.poll();
                if (v < last) {
                    outOfOrder++;
                }
                last = v;
                sum += v;
            }
        }
        return new long[] {sum, outOfOrder};
    }

    public static void main(String[] args) throws Exception {
        PriorityBlockingQueue<Integer> q = base();
        String[] names = {"size", "peek", "offer-poll-max", "offer-poll-min", "drain256"};
        long[] per = {N, N, PAIRS, PAIRS, DRAINS * 256L};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length + 1];
        for (int r = 0; r < REPS; r++) {
            long[] t = new long[names.length + 1];
            t[0] = System.nanoTime();
            sums[0] = size(q);
            t[1] = System.nanoTime();
            sums[1] = peek(q);
            t[2] = System.nanoTime();
            sums[2] = offerPollMax();
            t[3] = System.nanoTime();
            sums[3] = offerPollMin();
            t[4] = System.nanoTime();
            long[] d = drain256();
            t[5] = System.nanoTime();
            sums[4] = d[0];
            sums[5] = d[1];
            for (int i = 0; i < names.length; i++) {
                best[i] = Math.min(best[i], t[i + 1] - t[i]);
            }
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-15s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
        }
        System.out.println("checksum size=" + sums[0] + " peek=" + sums[1]
                + " offer-poll-max=" + sums[2] + " offer-poll-min=" + sums[3]
                + " drain256=" + sums[4] + " drain256-out-of-order=" + sums[5]);
        System.out.println("done");
    }
}
