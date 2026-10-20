// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 114, lane L1 -- the Java-visible shapes of a
// platform thread's TIMED and UNTIMED `LockSupport` parks, for the A/B of
// `CRATONVM_PARK_EVENT_LOCKLESS_WAKE` (default off; Windows only: the park
// waits on the thread's wake event and returns without re-taking the park
// state's mutex, as HotSpot's Windows `Parker::park`). Page:
// docs/internal/fixed-bugs/interpreter-i97-L1-process-exit-wake-latency-DONE-20261224.md,
// "Wave 114".
//
// Each row must print the same with the switch off and on, in both modes,
// JIT and --nojit:
//   * a permit given before the park makes parkNanos / park return at once;
//   * an unpark ends a parkNanos(10 s) and a park(), 20 rounds each;
//   * an interrupt ends both and leaves the flag set;
//   * the permit is consumed: the next parkNanos(60 ms) waits (>= 30 ms; a
//     Windows park may end on a clock tick before its time, HotSpot's too);
//   * 500 parkNanos(1 ms) raced by a thread that unparks without pause all
//     end, and afterwards no signal is left behind: once a permit is taken, a
//     parkNanos(60 ms) waits again (a stale wake event would end it at once).
//
// TIME lines (not compared): the median microseconds from the unpark to the
// woken thread's next instruction, timed and untimed (stdout, "TIME ").
//
// Run: java L1W114LocklessParkWakeShapes
//      cratonvm L1W114LocklessParkWakeShapes
//      CRATONVM_PARK_EVENT_LOCKLESS_WAKE=1 cratonvm L1W114LocklessParkWakeShapes
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, run in wave 115)
// permit first: timed=true untimed=true
// unpark wakes: timed=20 untimed=20
// interrupt wakes: timed=true flag=true untimed=true flag=true
// permit consumed: timed-waited=true untimed-then-timed-waited=true
// race rounds=500 ok=true
// no signal left: waited=true
// done
// HOTSPOT_EXPECTED_END

import java.util.Arrays;
import java.util.concurrent.locks.LockSupport;

public class L1W114LocklessParkWakeShapes {
    static final int ROUNDS = 20;
    static volatile int phase;
    static volatile long wokeAt;
    static volatile boolean stop;

    static void waitParked(Thread t) throws InterruptedException {
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (t.getState() != Thread.State.WAITING
                && t.getState() != Thread.State.TIMED_WAITING
                && System.nanoTime() < deadline) {
            Thread.onSpinWait();
        }
        // Past the VM's 2 ms running phase of a park: the blocking wait.
        Thread.sleep(5);
    }

    /** `ROUNDS` unpark wakes of a peer parked timed or untimed; returns the wakes. */
    static int wakeRows(boolean timed, long[] lat) throws Exception {
        phase = 0;
        int[] woken = new int[1];
        Thread peer = new Thread(() -> {
            for (int i = 0; i < ROUNDS; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                while (wokeAt != -1) {
                    if (timed) {
                        LockSupport.parkNanos(10_000_000_000L);
                    } else {
                        LockSupport.park();
                    }
                }
                wokeAt = System.nanoTime();
                woken[0]++;
                phase = 2 * i + 2;
            }
        });
        peer.start();
        for (int i = 0; i < ROUNDS; i++) {
            wokeAt = 0;
            phase = 2 * i + 1;
            waitParked(peer);
            long t0 = System.nanoTime();
            wokeAt = -1;
            LockSupport.unpark(peer);
            while (phase != 2 * i + 2) {
                Thread.onSpinWait();
            }
            lat[i] = wokeAt - t0;
        }
        peer.join();
        return woken[0];
    }

    static long median(long[] a) {
        long[] c = a.clone();
        Arrays.sort(c);
        return c[c.length / 2] / 1_000;
    }

    public static void main(String[] args) throws Exception {
        // 1. A permit first.
        Thread self = Thread.currentThread();
        LockSupport.unpark(self);
        long t0 = System.nanoTime();
        LockSupport.parkNanos(10_000_000_000L);
        boolean timedFirst = System.nanoTime() - t0 < 5_000_000_000L;
        LockSupport.unpark(self);
        t0 = System.nanoTime();
        LockSupport.park();
        boolean untimedFirst = System.nanoTime() - t0 < 5_000_000_000L;
        System.out.println("permit first: timed=" + timedFirst + " untimed=" + untimedFirst);

        // 2. Unpark wakes.
        long[] timedLat = new long[ROUNDS];
        long[] untimedLat = new long[ROUNDS];
        int timedWakes = wakeRows(true, timedLat);
        int untimedWakes = wakeRows(false, untimedLat);
        System.out.println("unpark wakes: timed=" + timedWakes + " untimed=" + untimedWakes);

        // 3. Interrupt wakes, the flag left set.
        boolean[] got = new boolean[4];
        for (int k = 0; k < 2; k++) {
            final boolean timed = k == 0;
            final int at = 2 * k;
            Thread p = new Thread(() -> {
                long s = System.nanoTime();
                while (!Thread.currentThread().isInterrupted()
                        && System.nanoTime() - s < 10_000_000_000L) {
                    if (timed) {
                        LockSupport.parkNanos(10_000_000_000L);
                    } else {
                        LockSupport.park();
                    }
                }
                got[at] = System.nanoTime() - s < 10_000_000_000L;
                got[at + 1] = Thread.currentThread().isInterrupted();
            });
            p.start();
            waitParked(p);
            p.interrupt();
            p.join();
        }
        System.out.println("interrupt wakes: timed=" + got[0] + " flag=" + got[1]
                + " untimed=" + got[2] + " flag=" + got[3]);

        // 4. The permit is consumed by the park it ended.
        boolean[] waited = new boolean[2];
        for (int k = 0; k < 2; k++) {
            final boolean timedFirstPark = k == 0;
            final int at = k;
            phase = 0;
            Thread p = new Thread(() -> {
                phase = 1;
                while (phase != 2) {
                    if (timedFirstPark) {
                        LockSupport.parkNanos(10_000_000_000L);
                    } else {
                        LockSupport.park();
                    }
                }
                long s = System.nanoTime();
                LockSupport.parkNanos(60_000_000L);
                waited[at] = System.nanoTime() - s >= 30_000_000L;
            });
            p.start();
            while (phase != 1) {
                Thread.onSpinWait();
            }
            waitParked(p);
            phase = 2;
            LockSupport.unpark(p);
            p.join();
        }
        System.out.println("permit consumed: timed-waited=" + waited[0]
                + " untimed-then-timed-waited=" + waited[1]);

        // 5. Unparks racing short timed parks, then no stale signal.
        stop = false;
        int[] rounds = new int[1];
        boolean[] leftClean = new boolean[1];
        Thread racer = new Thread(() -> {
            for (int i = 0; i < 500; i++) {
                LockSupport.parkNanos(1_000_000L);
                rounds[0]++;
            }
            stop = true;
            // Take any permit the last unparks left (they come before the
            // racer below sees `stop`), then park on a clean state.
            while (!quiet) {
                Thread.onSpinWait();
            }
            LockSupport.unpark(Thread.currentThread());
            LockSupport.parkNanos(1_000_000L);
            long s = System.nanoTime();
            LockSupport.parkNanos(60_000_000L);
            leftClean[0] = System.nanoTime() - s >= 30_000_000L;
        });
        quiet = false;
        racer.start();
        while (!stop) {
            LockSupport.unpark(racer);
        }
        quiet = true;
        racer.join();
        System.out.println("race rounds=" + rounds[0] + " ok=" + (rounds[0] == 500));
        System.out.println("no signal left: waited=" + leftClean[0]);

        System.out.println("TIME timed-wake median=" + median(timedLat));
        System.out.println("TIME untimed-wake median=" + median(untimedLat));
        System.out.println("done");
    }

    static volatile boolean quiet;
}
