// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 75, lane L6 -- bench probe for
// docs/internal/fixed-bugs/interpreter-i74-L6-proposal-sleep-without-the-10ms-poll-20261107-DONE-20261108.md:
//
//   CRATONVM_SLEEP_NO_POLL=1  a platform thread's Thread.sleep waits on its
//                             park state's condvar for the whole remainder
//                             (not 10 ms std::thread::sleep slices), and an
//                             interrupt wakes it at once.
//
// A/B, one binary, interleaved, medians of the per-run values:
//   cratonvm L6W75SleepBench
//   CRATONVM_SLEEP_NO_POLL=1 cratonvm L6W75SleepBench
// and the same with --compatible (whose Thread.sleep(long) is the VM's
// sleep(J) native; --jdk-only runs the JDK body down to sleepNanos0). On the
// Linux host also compare the stderr line "ctxt-switches idle-window=N"
// (sum over /proc/self/task/*/status): expected ~200 sleepers x 100 slices/s
// x 2 s = ~40 000 off, near 0 on.
//
// Rows (stdout):
//   idle-cpu-base     process CPU ns per second while main sleeps 2 s with no
//                     other sleeper (the VM's own background);
//   idle-cpu          the same with SLEEPERS threads in Thread.sleep(60 s), ns
//                     of process CPU per sleeper per second, base subtracted
//                     (should drop to ~0 on);
//   interrupt-sleep   median ns from Thread.interrupt() to the sleeper's
//                     InterruptedException handler running (off: up to one
//                     10 ms slice; on: tens of microseconds);
//   sleep-1ms         mean real length of Thread.sleep(1), ns (Windows: the
//                     slice rounds up to the 15.6 ms tick off, ~1 ms on);
//   sleep-0-1ms-nanos mean real length of Thread.sleep(0, 500_000), ns.
//
// The lines without "ns/" are behaviour, identical on every VM and every arm:
// an interrupted sleep throws "sleep interrupted" and clears the flag; an
// unpark neither ends a sleep nor is consumed by it (the next park returns at
// once on the kept permit).
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// interrupt message=sleep interrupted flag-after=false
// unpark-during-sleep ends-sleep=false permit-kept=true
// pre-interrupted sleep(0) throws=true flag-after=false
// checksum sleepers=200 interrupts=200 sleeps=200
// done
// HOTSPOT_EXPECTED_END
import java.io.IOException;
import java.lang.management.ManagementFactory;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;
import java.util.stream.Stream;

public class L6W75SleepBench {
    static final int SLEEPERS = 200;
    static final long IDLE_MS = 2_000;
    static final int INTERRUPTS = 200;
    static final int SLEEPS = 200;

    static volatile long caughtAt;
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

    /** CPU ns per second of an idle window (main parks; not a sleep under test). */
    static double idleCpuPerSecond() {
        long c0 = processCpuNanos();
        long t0 = System.nanoTime();
        long end = t0 + IDLE_MS * 1_000_000L;
        long now;
        while ((now = System.nanoTime()) < end) {
            LockSupport.parkNanos(end - now);
        }
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

        AtomicInteger asleep = new AtomicInteger();
        CountDownLatch done = new CountDownLatch(SLEEPERS);
        Thread[] sleepers = new Thread[SLEEPERS];
        for (int i = 0; i < SLEEPERS; i++) {
            sleepers[i] = new Thread(() -> {
                asleep.incrementAndGet();
                try {
                    Thread.sleep(60_000);
                } catch (InterruptedException e) {
                    // released
                }
                done.countDown();
            }, "w75-idle-" + i);
            sleepers[i].setDaemon(true);
            sleepers[i].start();
        }
        while (asleep.get() < SLEEPERS) {
            Thread.sleep(1);
        }
        Thread.sleep(200);
        long s0 = contextSwitches();
        double loaded = idleCpuPerSecond();
        long s1 = contextSwitches();
        double perSleeper = (base < 0 || loaded < 0) ? -1 : (loaded - base) / SLEEPERS;
        System.out.printf("%-18s %.1f ns/op%n", "idle-cpu", perSleeper);
        if (s0 >= 0 && s1 >= 0) {
            System.err.println("ctxt-switches idle-window=" + (s1 - s0));
        }
        for (Thread t : sleepers) {
            t.interrupt();
        }
        done.await();
        for (Thread t : sleepers) {
            t.join();
        }
        return asleep.get();
    }

    /** Median ns from interrupt() to the sleeper's handler; behaviour line. */
    static int interruptRow() throws InterruptedException {
        long[] lat = new long[INTERRUPTS];
        AtomicInteger caught = new AtomicInteger();
        String[] message = new String[1];
        boolean[] flagAfter = new boolean[1];
        phase = 0;
        Thread sleeper = new Thread(() -> {
            for (int i = 0; i < INTERRUPTS; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                try {
                    Thread.sleep(60_000);
                } catch (InterruptedException e) {
                    caughtAt = System.nanoTime();
                    if (i == 0) {
                        message[0] = e.getMessage();
                        flagAfter[0] = Thread.currentThread().isInterrupted();
                    }
                    caught.incrementAndGet();
                }
                phase = 2 * i + 2;
            }
        }, "w75-interrupt-sleeper");
        sleeper.start();
        for (int i = 0; i < INTERRUPTS; i++) {
            phase = 2 * i + 1;
            // Well into the sleep.
            Thread.sleep(3);
            long t0 = System.nanoTime();
            sleeper.interrupt();
            while (phase != 2 * i + 2) {
                Thread.onSpinWait();
            }
            lat[i] = caughtAt - t0;
        }
        sleeper.join();
        Arrays.sort(lat);
        System.out.printf("%-18s %.1f ns/op%n", "interrupt-sleep", (double) lat[INTERRUPTS / 2]);
        System.out.println("interrupt message=" + message[0] + " flag-after=" + flagAfter[0]);
        return caught.get();
    }

    static int sleepRows() throws InterruptedException {
        long total = 0;
        for (int i = 0; i < SLEEPS; i++) {
            long t0 = System.nanoTime();
            Thread.sleep(1);
            total += System.nanoTime() - t0;
        }
        System.out.printf("%-18s %.1f ns/op%n", "sleep-1ms", total / (double) SLEEPS);
        total = 0;
        for (int i = 0; i < SLEEPS; i++) {
            long t0 = System.nanoTime();
            Thread.sleep(0, 500_000);
            total += System.nanoTime() - t0;
        }
        System.out.printf("%-18s %.1f ns/op%n", "sleep-0-1ms-nanos", total / (double) SLEEPS);
        return SLEEPS;
    }

    /** An unpark during a sleep: the sleep runs out, the permit is kept. */
    static void unparkDuringSleep() throws InterruptedException {
        boolean[] endedEarly = new boolean[1];
        boolean[] permitKept = new boolean[1];
        CountDownLatch asleep = new CountDownLatch(1);
        Thread t = new Thread(() -> {
            asleep.countDown();
            long t0 = System.nanoTime();
            try {
                Thread.sleep(200);
            } catch (InterruptedException e) {
                // not expected
            }
            endedEarly[0] = System.nanoTime() - t0 < 190_000_000L;
            long p0 = System.nanoTime();
            LockSupport.parkNanos(2_000_000_000L);
            permitKept[0] = System.nanoTime() - p0 < 1_000_000_000L;
        }, "w75-unpark-sleeper");
        t.start();
        asleep.await();
        Thread.sleep(30);
        LockSupport.unpark(t);
        t.join();
        System.out.println("unpark-during-sleep ends-sleep=" + endedEarly[0]
                + " permit-kept=" + permitKept[0]);
    }

    static void preInterruptedSleepZero() {
        Thread.currentThread().interrupt();
        boolean threw = false;
        try {
            Thread.sleep(0);
        } catch (InterruptedException e) {
            threw = true;
        }
        System.out.println("pre-interrupted sleep(0) throws=" + threw
                + " flag-after=" + Thread.currentThread().isInterrupted());
        Thread.interrupted();
    }

    public static void main(String[] args) throws Exception {
        int sleepers = idleRows();
        int interrupts = interruptRow();
        int sleeps = sleepRows();
        unparkDuringSleep();
        preInterruptedSleepZero();
        System.out.println("checksum sleepers=" + sleepers + " interrupts=" + interrupts
                + " sleeps=" + sleeps);
        System.out.println("done");
    }
}
