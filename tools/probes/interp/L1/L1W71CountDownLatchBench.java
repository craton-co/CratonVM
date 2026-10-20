// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L1 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i70-L1-proposal-a-countdownlatch-family-row-DONE-20261104.md
// (the `JdkCountDownLatch` row, `CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH`: over a
// real JDK `--compatible` runs the JDK 25 `CountDownLatch` bodies -- AQS CAS on
// the `Sync` state, `LockSupport.park(this)` -- instead of the stand-ins, which
// kept the count in an `int[]` holder under the latch's monitor and waited in
// 10 ms monitor-wait slices).
//
// A/B, one binary, `--compatible` over a real JDK:
//   cratonvm --compatible L1W71CountDownLatchBench                                       (row on, default)
//   CRATONVM_COMPAT_JDK_COUNT_DOWN_LATCH=0 cratonvm --compatible L1W71CountDownLatchBench (stand-ins)
// and `cratonvm L1W71CountDownLatchBench` (jdk-only) as the reference the
// row-on arm should equal. Interleave the arms and take medians of the per-run
// minimums (in-JVM timings swing ~3x on a noisy host). Add `--nojit` for the
// interpreter-only picture.
//
// Rows (stdout, min ns/op over REPS=7 rounds):
//   create-count-await  new CountDownLatch(1); countDown(); await() (uncontended);
//   await-open          await() on a latch already at zero;
//   count-down-open     countDown() on a latch already at zero;
//   get-count           getCount();
//   timed-await-open    await(1, SECONDS) on a latch already at zero;
//   handoff             two threads ping-pong through fresh latches, ns per round trip;
//   fanout8             8 parked waiters released by one countDown, then 8
//                       countDowns release the main thread; ns per round.
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum create=200000 await-open=200000 count-down-open=200000 get-count=0 timed-await-open=200000 handoff=2000 fanout8=4000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

public class L1W71CountDownLatchBench {
    static final int REPS = 7;
    static final int N = 200_000;
    static final int HANDOFF = 2_000;
    static final int FAN = 8;
    static final int FAN_ROUNDS = 500;

    static long createCountAwait() throws InterruptedException {
        long n = 0;
        for (int i = 0; i < N; i++) {
            CountDownLatch l = new CountDownLatch(1);
            l.countDown();
            l.await();
            n += 1 - l.getCount();
        }
        return n;
    }

    static long awaitOpen(CountDownLatch open) throws InterruptedException {
        long n = 0;
        for (int i = 0; i < N; i++) {
            open.await();
            n++;
        }
        return n;
    }

    static long countDownOpen(CountDownLatch open) {
        long n = 0;
        for (int i = 0; i < N; i++) {
            open.countDown();
            n++;
        }
        return n;
    }

    static long getCount(CountDownLatch open) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += open.getCount();
        }
        return sum;
    }

    static long timedAwaitOpen(CountDownLatch open) throws InterruptedException {
        long n = 0;
        for (int i = 0; i < N; i++) {
            if (open.await(1, TimeUnit.SECONDS)) {
                n++;
            }
        }
        return n;
    }

    static long handoff() throws InterruptedException {
        CountDownLatch[] ping = new CountDownLatch[HANDOFF];
        CountDownLatch[] pong = new CountDownLatch[HANDOFF];
        for (int i = 0; i < HANDOFF; i++) {
            ping[i] = new CountDownLatch(1);
            pong[i] = new CountDownLatch(1);
        }
        long[] seen = {0};
        Thread peer = new Thread(() -> {
            try {
                for (int i = 0; i < HANDOFF; i++) {
                    ping[i].await();
                    seen[0]++;
                    pong[i].countDown();
                }
            } catch (InterruptedException e) {
                seen[0] = -1;
            }
        }, "handoff-peer");
        peer.start();
        for (int i = 0; i < HANDOFF; i++) {
            ping[i].countDown();
            pong[i].await();
        }
        peer.join();
        return seen[0];
    }

    static long fanout() throws InterruptedException {
        CountDownLatch[] gate = new CountDownLatch[FAN_ROUNDS];
        CountDownLatch[] done = new CountDownLatch[FAN_ROUNDS];
        for (int r = 0; r < FAN_ROUNDS; r++) {
            gate[r] = new CountDownLatch(1);
            done[r] = new CountDownLatch(FAN);
        }
        Thread[] ts = new Thread[FAN];
        long[] passes = new long[FAN];
        for (int t = 0; t < FAN; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                try {
                    for (int r = 0; r < FAN_ROUNDS; r++) {
                        gate[r].await();
                        passes[id]++;
                        done[r].countDown();
                    }
                } catch (InterruptedException e) {
                    passes[id] = -1;
                }
            }, "fanout-" + t);
            ts[t].start();
        }
        for (int r = 0; r < FAN_ROUNDS; r++) {
            gate[r].countDown();
            done[r].await();
        }
        for (Thread t : ts) {
            t.join();
        }
        long sum = 0;
        for (long p : passes) {
            sum += p;
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        CountDownLatch open = new CountDownLatch(0);
        String[] names = {
            "create-count-await", "await-open", "count-down-open", "get-count",
            "timed-await-open", "handoff", "fanout8",
        };
        long[] per = {N, N, N, N, N, HANDOFF, FAN_ROUNDS};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        for (int r = 0; r < REPS; r++) {
            long[] t = new long[names.length + 1];
            t[0] = System.nanoTime();
            sums[0] = createCountAwait();
            t[1] = System.nanoTime();
            sums[1] = awaitOpen(open);
            t[2] = System.nanoTime();
            sums[2] = countDownOpen(open);
            t[3] = System.nanoTime();
            sums[3] = getCount(open);
            t[4] = System.nanoTime();
            sums[4] = timedAwaitOpen(open);
            t[5] = System.nanoTime();
            sums[5] = handoff();
            t[6] = System.nanoTime();
            sums[6] = fanout();
            t[7] = System.nanoTime();
            for (int i = 0; i < names.length; i++) {
                best[i] = Math.min(best[i], t[i + 1] - t[i]);
            }
        }
        for (int i = 0; i < names.length; i++) {
            System.out.printf("%-19s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
        }
        System.out.println("checksum create=" + sums[0] + " await-open=" + sums[1]
                + " count-down-open=" + sums[2] + " get-count=" + sums[3]
                + " timed-await-open=" + sums[4] + " handoff=" + sums[5]
                + " fanout8=" + sums[6]);
        System.out.println("done");
    }
}
