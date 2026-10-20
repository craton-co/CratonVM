// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 81, lane L6 -- bench for
// docs/internal/fixed-bugs/interpreter-i77-L6-proposal-adaptive-lazy-park-budget-DONE-20261115.md
// (`CRATONVM_PARK_LAZY_ADAPTIVE`, default off). Rows:
//
//   long-park-3ms cpu-per-park   64 threads loop LockSupport.parkNanos(3 ms) for
//                                3 s; the threads' own CPU time per park
//                                (ThreadMXBean.getCurrentThreadCpuTime). Each
//                                park outlasts the 2 ms running phase, so the
//                                phase is pure cost there: the switch skips it.
//   long-park-3ms wall-per-park  the same run: wall time x threads / parks
//                                (how late the parks wake; ~3 ms ideal).
//   park-pingpong                two threads hand a turn back and forth with
//                                park / unpark (a handoff: must not regress).
//   gc-64-parked                 System.gc() with 64 threads in untimed parks
//                                (their parks reach the GC-blocked region at
//                                once with the switch, after 2 ms without).
//
// Timing lines carry "ns/op"; no other line is printed but "done".
//
// A/B (both modes, interleave the runs, take medians):
//   cratonvm L6W81LongParkBench
//   CRATONVM_PARK_LAZY_ADAPTIVE=1 cratonvm L6W81LongParkBench
//   cratonvm --compatible L6W81LongParkBench
//   CRATONVM_PARK_LAZY_ADAPTIVE=1 cratonvm --compatible L6W81LongParkBench
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// done
// HOTSPOT_EXPECTED_END
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.locks.LockSupport;

public class L6W81LongParkBench {
    static final int THREADS = 64;
    static final ThreadMXBean MX = ManagementFactory.getThreadMXBean();

    static void longParkRow(long runNanos) throws InterruptedException {
        long[] parks = new long[THREADS];
        long[] cpu = new long[THREADS];
        CountDownLatch ready = new CountDownLatch(THREADS);
        CountDownLatch go = new CountDownLatch(1);
        Thread[] ts = new Thread[THREADS];
        for (int i = 0; i < THREADS; i++) {
            final int k = i;
            ts[i] = new Thread(() -> {
                ready.countDown();
                try {
                    go.await();
                } catch (InterruptedException e) {
                    return;
                }
                long cpu0 = MX.getCurrentThreadCpuTime();
                long end = System.nanoTime() + runNanos;
                long n = 0;
                while (System.nanoTime() < end) {
                    LockSupport.parkNanos(3_000_000L);
                    n++;
                }
                parks[k] = n;
                cpu[k] = MX.getCurrentThreadCpuTime() - cpu0;
            }, "w81-long-park-" + i);
            ts[i].setDaemon(true);
            ts[i].start();
        }
        ready.await();
        long t0 = System.nanoTime();
        go.countDown();
        for (Thread t : ts) {
            t.join();
        }
        long wall = System.nanoTime() - t0;
        long totalParks = Arrays.stream(parks).sum();
        long totalCpu = Arrays.stream(cpu).sum();
        if (totalParks == 0) {
            totalParks = 1;
        }
        System.out.println("long-park-3ms cpu-per-park " + (totalCpu / totalParks) + " ns/op");
        System.out.println("long-park-3ms wall-per-park " + (wall * THREADS / totalParks) + " ns/op");
    }

    static volatile int turn;

    static void pingPongRow(int rounds) throws InterruptedException {
        Thread[] pair = new Thread[2];
        CountDownLatch done = new CountDownLatch(2);
        long[] took = new long[1];
        for (int side = 0; side < 2; side++) {
            final int me = side;
            pair[side] = new Thread(() -> {
                long t0 = System.nanoTime();
                for (int r = 0; r < rounds; r++) {
                    while (turn != me) {
                        LockSupport.park();
                    }
                    turn = 1 - me;
                    LockSupport.unpark(pair[1 - me]);
                }
                if (me == 0) {
                    took[0] = System.nanoTime() - t0;
                }
                done.countDown();
            }, "w81-pingpong-" + side);
            pair[side].setDaemon(true);
        }
        turn = 0;
        pair[0].start();
        pair[1].start();
        if (!done.await(30, TimeUnit.SECONDS)) {
            LockSupport.unpark(pair[0]);
            LockSupport.unpark(pair[1]);
        }
        System.out.println("park-pingpong " + (took[0] / Math.max(1, rounds)) + " ns/op");
    }

    static void gcRow(int collections) throws InterruptedException {
        AtomicBoolean stop = new AtomicBoolean();
        CountDownLatch parked = new CountDownLatch(THREADS);
        Thread[] ts = new Thread[THREADS];
        for (int i = 0; i < THREADS; i++) {
            ts[i] = new Thread(() -> {
                parked.countDown();
                while (!stop.get()) {
                    LockSupport.park();
                }
            }, "w81-gc-parked-" + i);
            ts[i].setDaemon(true);
            ts[i].start();
        }
        parked.await();
        Thread.sleep(50);
        long[] each = new long[collections];
        for (int c = 0; c < collections; c++) {
            long t0 = System.nanoTime();
            System.gc();
            each[c] = System.nanoTime() - t0;
            Thread.sleep(20);
        }
        stop.set(true);
        for (Thread t : ts) {
            LockSupport.unpark(t);
        }
        for (Thread t : ts) {
            t.join(2000);
        }
        Arrays.sort(each);
        System.out.println("gc-64-parked " + each[collections / 2] + " ns/op");
    }

    public static void main(String[] args) throws Exception {
        if (MX.isCurrentThreadCpuTimeSupported() && !MX.isThreadCpuTimeEnabled()) {
            MX.setThreadCpuTimeEnabled(true);
        }
        // Warm-up, then three reps of each row.
        longParkRow(500_000_000L);
        pingPongRow(20_000);
        for (int rep = 0; rep < 3; rep++) {
            longParkRow(3_000_000_000L);
            pingPongRow(100_000);
            gcRow(20);
        }
        System.out.println("done");
    }
}
