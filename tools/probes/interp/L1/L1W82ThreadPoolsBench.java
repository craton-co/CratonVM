// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 82, lane L1 -- bench for the `JdkThreadPools`
// HotSpot-alignment row (`CRATONVM_COMPAT_JDK_THREAD_POOLS`, default OFF).
// With the row a real-JDK `--compatible` VM registers no native on
// `ThreadPoolExecutor`, so each call below is the JDK body with no door in
// front; without it every call reaches a registered router (a retired row
// re-tagged `SyntheticStub`, or `execute`'s stated stub) that the door then
// yields to the same JDK body, or that hands the pool to it through
// `invoke_special_bytecode_only`. The row's case is this cost.
//
// A/B, one binary, interleaved, >= 5 rounds, medians of the per-run minima, JIT
// and `--nojit`:
//   cratonvm --compatible L1W82ThreadPoolsBench                                   (row off)
//   CRATONVM_COMPAT_JDK_THREAD_POOLS=1 cratonvm --compatible L1W82ThreadPoolsBench (row on)
//   cratonvm L1W82ThreadPoolsBench                                                (--jdk-only: the JDK bodies)
//   java L1W82ThreadPoolsBench                                                    (HotSpot)
// One row only: `L1W82ThreadPoolsBench <row>`.
//
// Rows (stdout, min ns/op over REPS=5 rounds):
//   is-shutdown       pool.isShutdown() (a `ctl` read)
//   is-terminated     ((ExecutorService) pool).isTerminated() through the interface
//   pool-size         pool.getPoolSize() (takes `mainLock`)
//   core-size         pool.getCorePoolSize() (a field read)
//   completed-count   pool.getCompletedTaskCount() (takes `mainLock`, walks the workers)
//   execute-drain     execute(no-op) x 64 on a 2-worker pool, then wait for all 64 (per task)
//   submit-get        submit(Callable).get() on a 1-worker pool (park / unpark dominated)
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum is-shutdown=0 is-terminated=0 pool-size=400000 core-size=400000 completed-count=0 execute-drain=128000 submit-get=199990000
// done
// HOTSPOT_EXPECTED_END
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

public class L1W82ThreadPoolsBench {
    static final int REPS = 5;
    static final int N = 200_000;
    static final int BATCHES = 2_000;
    static final int BATCH = 64;
    static final int ASYNC_N = 20_000;

    static ThreadPoolExecutor pool(int n) {
        return new ThreadPoolExecutor(n, n, 0L, TimeUnit.MILLISECONDS, new LinkedBlockingQueue<Runnable>());
    }

    /** A 2-worker pool with both workers started and idle. */
    static ThreadPoolExecutor warmPool() {
        ThreadPoolExecutor p = pool(2);
        p.prestartAllCoreThreads();
        return p;
    }

    static long isShutdown(ThreadPoolExecutor p) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            if (p.isShutdown()) {
                sum++;
            }
        }
        return sum;
    }

    static long isTerminated(ExecutorService e) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            if (e.isTerminated()) {
                sum++;
            }
        }
        return sum;
    }

    static long poolSize(ThreadPoolExecutor p) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += p.getPoolSize();
        }
        return sum;
    }

    static long coreSize(ThreadPoolExecutor p) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += p.getCorePoolSize();
        }
        return sum;
    }

    static long completedCount(ThreadPoolExecutor p) {
        long sum = 0;
        for (int i = 0; i < N; i++) {
            sum += p.getCompletedTaskCount();
        }
        return sum;
    }

    static long executeDrain(ThreadPoolExecutor p) throws InterruptedException {
        AtomicLong ran = new AtomicLong();
        Runnable task = ran::incrementAndGet;
        for (int b = 0; b < BATCHES; b++) {
            CountDownLatch done = new CountDownLatch(BATCH);
            Runnable counted = () -> {
                task.run();
                done.countDown();
            };
            for (int i = 0; i < BATCH; i++) {
                p.execute(counted);
            }
            done.await();
        }
        return ran.get();
    }

    static long submitGet(ThreadPoolExecutor p) throws Exception {
        long sum = 0;
        for (int i = 0; i < ASYNC_N; i++) {
            final long v = i;
            sum += p.submit(() -> v).get();
        }
        return sum;
    }

    public static void main(String[] args) throws Exception {
        String only = args.length > 0 ? args[0] : null;
        String[] names = {"is-shutdown", "is-terminated", "pool-size", "core-size", "completed-count",
            "execute-drain", "submit-get"};
        long[] per = {N, N, N, N, N, (long) BATCHES * BATCH, ASYNC_N};
        long[] best = new long[names.length];
        java.util.Arrays.fill(best, Long.MAX_VALUE);
        long[] sums = new long[names.length];
        boolean[] ran = new boolean[names.length];
        ThreadPoolExecutor idle = warmPool();
        ThreadPoolExecutor busy = warmPool();
        ThreadPoolExecutor one = pool(1);
        for (int rep = 0; rep < REPS; rep++) {
            for (int i = 0; i < names.length; i++) {
                if (only != null && !only.equals(names[i])) {
                    continue;
                }
                long t0 = System.nanoTime();
                long s;
                try {
                    switch (i) {
                        case 0: s = isShutdown(idle); break;
                        case 1: s = isTerminated(idle); break;
                        case 2: s = poolSize(idle); break;
                        case 3: s = coreSize(idle); break;
                        case 4: s = completedCount(idle); break;
                        case 5: s = executeDrain(busy); break;
                        default: s = submitGet(one); break;
                    }
                } catch (Throwable t) {
                    if (rep == 0) {
                        System.out.println(names[i] + " failed: " + t);
                    }
                    continue;
                }
                best[i] = Math.min(best[i], System.nanoTime() - t0);
                sums[i] = s;
                ran[i] = true;
            }
        }
        idle.shutdown();
        busy.shutdown();
        one.shutdown();
        idle.awaitTermination(10, TimeUnit.SECONDS);
        busy.awaitTermination(10, TimeUnit.SECONDS);
        one.awaitTermination(10, TimeUnit.SECONDS);
        StringBuilder sb = new StringBuilder("checksum");
        for (int i = 0; i < names.length; i++) {
            if (!ran[i]) {
                continue;
            }
            System.out.printf("%-15s %.1f ns/op%n", names[i], best[i] / (double) per[i]);
            sb.append(' ').append(names[i]).append('=').append(sums[i]);
        }
        System.out.println(sb);
        System.out.println("done");
    }
}
