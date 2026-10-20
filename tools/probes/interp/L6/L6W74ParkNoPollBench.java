// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 74, lane L6 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i73-L6-proposal-untimed-park-without-a-5ms-poll-DONE-20261115.md:
//
//   CRATONVM_PARK_UNTIMED_NO_POLL=1  a blocking park waits without the 5 ms
//                                    interrupt poll (untimed: until woken;
//                                    timed: one wait of the remainder), and an
//                                    interrupt wakes the park state itself.
//
// A/B, one binary, interleaved, medians of the per-run values:
//   cratonvm L6W74ParkNoPollBench
//   CRATONVM_PARK_UNTIMED_NO_POLL=1 cratonvm L6W74ParkNoPollBench
// and the same with --compatible. On the Linux host, also compare the
// process's context switches (stderr line "ctxt-switches", read from
// /proc/self/task/*/status around the idle window): expected ~200 parkers x
// 200 polls/s x 2 s = ~80 000 off, near 0 on.
//
// Rows (stdout):
//   idle-cpu-base     process CPU time while main sleeps 2 s with no parked
//                     threads, ns per second (the VM's own background);
//   idle-cpu          the same with PARKERS threads in an untimed park, ns of
//                     process CPU per parked thread per second, the base
//                     subtracted: the poll's cost (should drop to ~0 on);
//   unpark-wake       median ns from unpark() to the parked thread running,
//                     a park well past the running phase (must not regress);
//   interrupt-wake    the same for Thread.interrupt() (must not regress);
//   timed-park-1ms    mean actual length of parkNanos(1 ms), ns (accuracy of
//                     the one-wait timed path; must not grow);
//   park-with-permit  unpark(self); park() on one thread, ns per pair (the
//                     wave-73 off-state row; must not move with the switch).
//
// The lines without "ns/" are checksums, identical on every VM and every arm:
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum parkers=200 unpark-wakes=200 interrupt-wakes=200 timed=200 permit=200000
// done
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;
import java.util.stream.Stream;

public class L6W74ParkNoPollBench {
    static final int PARKERS = 200;
    static final long IDLE_MS = 2_000;
    static final int WAKES = 200;
    static final int TIMED = 200;
    static final int PERMITS = 200_000;
    static final int REPS = 5;

    static volatile boolean stop;
    static volatile long wokeAt;
    static volatile int phase;

    /** Process CPU time in ns, or -1 when the platform bean has none. */
    static long processCpuNanos() {
        try {
            Object os = ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean bean) {
                return bean.getProcessCpuTime();
            }
        } catch (Throwable t) {
            // fall through
        }
        return -1;
    }

    /** Sum of every thread's context switches (Linux), or -1. */
    static long contextSwitches() {
        Path tasks = Path.of("/proc/self/task");
        if (!Files.isDirectory(tasks)) {
            return -1;
        }
        long sum = 0;
        try (Stream<Path> list = Files.list(tasks)) {
            for (Path task : (Iterable<Path>) list::iterator) {
                try {
                    for (String line : Files.readAllLines(task.resolve("status"))) {
                        if (line.startsWith("voluntary_ctxt_switches:")
                                || line.startsWith("nonvoluntary_ctxt_switches:")) {
                            sum += Long.parseLong(line.substring(line.indexOf(':') + 1).trim());
                        }
                    }
                } catch (IOException | RuntimeException e) {
                    // a thread that exited meanwhile
                }
            }
        } catch (IOException e) {
            return -1;
        }
        return sum;
    }

    /** CPU ns per second of an idle window (main sleeps). */
    static double idleCpuPerSecond() throws InterruptedException {
        long c0 = processCpuNanos();
        long t0 = System.nanoTime();
        Thread.sleep(IDLE_MS);
        long c1 = processCpuNanos();
        long t1 = System.nanoTime();
        if (c0 < 0 || c1 < 0) {
            return -1;
        }
        return (c1 - c0) * 1e9 / (t1 - t0);
    }

    static int idleRows() throws InterruptedException {
        double base = idleCpuPerSecond();
        System.out.printf("%-18s %.1f ns/op%n", "idle-cpu-base", base);

        stop = false;
        AtomicInteger parked = new AtomicInteger();
        Thread[] parkers = new Thread[PARKERS];
        for (int i = 0; i < PARKERS; i++) {
            parkers[i] = new Thread(() -> {
                parked.incrementAndGet();
                while (!stop) {
                    LockSupport.park();
                }
            }, "w74-idle-" + i);
            parkers[i].setDaemon(true);
            parkers[i].start();
        }
        while (parked.get() < PARKERS) {
            Thread.sleep(1);
        }
        // Past every running phase: all of them in the blocking wait.
        Thread.sleep(200);
        long s0 = contextSwitches();
        double loaded = idleCpuPerSecond();
        long s1 = contextSwitches();
        double perParker = (base < 0 || loaded < 0) ? -1 : (loaded - base) / PARKERS;
        System.out.printf("%-18s %.1f ns/op%n", "idle-cpu", perParker);
        if (s0 >= 0 && s1 >= 0) {
            System.err.println("ctxt-switches idle-window=" + (s1 - s0));
        }
        stop = true;
        for (Thread t : parkers) {
            LockSupport.unpark(t);
        }
        for (Thread t : parkers) {
            t.join();
        }
        return parked.get();
    }

    /** Median ns from the wake call to the woken thread running. */
    static int wakeRow(String row, boolean interrupt) throws InterruptedException {
        long[] lat = new long[WAKES];
        AtomicInteger woken = new AtomicInteger();
        phase = 0;
        Thread peer = new Thread(() -> {
            for (int i = 0; i < WAKES; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                // Park until the wake of this round (a spurious return parks again).
                while (phase == 2 * i + 1) {
                    LockSupport.park();
                    if (interrupt) {
                        if (Thread.interrupted()) {
                            break;
                        }
                    } else if (wokeAt == -1) {
                        break;
                    }
                }
                wokeAt = System.nanoTime();
                woken.incrementAndGet();
                phase = 2 * i + 2;
            }
        }, "w74-" + row);
        peer.start();
        for (int i = 0; i < WAKES; i++) {
            wokeAt = 0;
            phase = 2 * i + 1;
            // Well past the 2 ms running phase: the blocking wait.
            Thread.sleep(5);
            long t0 = System.nanoTime();
            if (interrupt) {
                peer.interrupt();
            } else {
                wokeAt = -1;
                LockSupport.unpark(peer);
            }
            while (phase != 2 * i + 2) {
                Thread.onSpinWait();
            }
            lat[i] = wokeAt - t0;
        }
        peer.join();
        Arrays.sort(lat);
        System.out.printf("%-18s %.1f ns/op%n", row, (double) lat[WAKES / 2]);
        return woken.get();
    }

    static int timedRow() {
        long total = 0;
        for (int i = 0; i < TIMED; i++) {
            long t0 = System.nanoTime();
            LockSupport.parkNanos(1_000_000L);
            total += System.nanoTime() - t0;
        }
        System.out.printf("%-18s %.1f ns/op%n", "timed-park-1ms", total / (double) TIMED);
        return TIMED;
    }

    static long permitRow() {
        Thread me = Thread.currentThread();
        long best = Long.MAX_VALUE;
        long pairs = 0;
        for (int r = 0; r < REPS; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < PERMITS; i++) {
                LockSupport.unpark(me);
                LockSupport.park();
            }
            best = Math.min(best, System.nanoTime() - t0);
            pairs += PERMITS;
        }
        System.out.printf("%-18s %.1f ns/op%n", "park-with-permit", best / (double) PERMITS);
        return pairs / REPS;
    }

    public static void main(String[] args) throws Exception {
        int parkers = idleRows();
        int unparkWakes = wakeRow("unpark-wake", false);
        int interruptWakes = wakeRow("interrupt-wake", true);
        int timed = timedRow();
        long permit = permitRow();
        System.out.println("checksum parkers=" + parkers + " unpark-wakes=" + unparkWakes
                + " interrupt-wakes=" + interruptWakes + " timed=" + timed + " permit=" + permit);
        System.out.println("done");
    }
}
