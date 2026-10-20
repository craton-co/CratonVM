// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 77, lane L6 -- does the waker's own CPU decide
// the wake tail of CRATONVM_PARK_WAKE_AFTER_UNLOCK? Page:
// docs/internal/fixed-bugs/interpreter-i76-L6-proposal-one-park-wait-and-one-unpark-20261109-DONE-20261110.md
// ("Progress (wave 77)": the ms-scale twice/interrupt p90 of
// L6W76ParkWakeBench under CRATONVM_PARK_WAKE_AFTER_UNLOCK=1).
//
// L6W76ParkWakeBench's waker SPINS (Thread.onSpinWait, one PAUSE) from the
// wake call until the peer has run. With the default unpark the twice and
// interrupt rows take the peer's park-state mutex a second time; that lock
// meets the mutex parking_lot handed to the woken peer and BLOCKS (spin,
// sched_yield, futex wait), which gives the waker's CPU away. The after-unlock
// shape never blocks the waker, so a peer that was queued behind a busy CPU
// waits for the scheduler. This bench runs the same three wakes with the
// waker waiting three ways:
//   spin   as L6W76ParkWakeBench (onSpinWait);
//   yield  Thread.yield() in the loop (gives the CPU away every round);
//   block  LockSupport.park() until the peer unparks it (no CPU at all).
// If the after-unlock tails are ms-scale only in the spin rows, the wake is
// not lost: the peer is waiting for a CPU the default arm's blocking second
// lock happened to free.
//
// A/B, one binary, the arms interleaved, >= 7 reps, medians of each row;
// --jdk-only and --compatible:
//   cratonvm L6W77ParkWakeWakerBench
//   CRATONVM_PARK_WAKE_AFTER_UNLOCK=1 cratonvm L6W77ParkWakeWakerBench
//   CRATONVM_PARK_UNTIMED_NO_POLL=1 CRATONVM_PARK_WAKE_AFTER_UNLOCK=1 cratonvm L6W77ParkWakeWakerBench
// Also run each arm under `taskset -c 0` (one CPU: every row should then show
// the spin rows' tail in every arm) and note `nproc` and the host load.
//
// Rows (stdout; percentile of WAKES samples, ns from the wake call to the
// woken peer running): <wake>-<waiter>-p50 / -p90 for wake in unpark, twice,
// interrupt and waiter in spin, yield, block.
// Stderr: the processor count.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum woken=1800
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.locks.LockSupport;

public class L6W77ParkWakeWakerBench {
    static final int WAKES = 200;

    static volatile long wokeAt;
    static volatile int phase;

    enum Kind { UNPARK, TWICE, INTERRUPT }
    enum Waiter { SPIN, YIELD, BLOCK }

    static int wakeRow(Kind kind, Waiter waiter) throws InterruptedException {
        String row = kind.name().toLowerCase() + "-" + waiter.name().toLowerCase();
        long[] lat = new long[WAKES];
        int[] woken = new int[1];
        Thread main = Thread.currentThread();
        phase = 0;
        Thread peer = new Thread(() -> {
            for (int i = 0; i < WAKES; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                while (phase == 2 * i + 1) {
                    LockSupport.park();
                    if (kind == Kind.INTERRUPT) {
                        if (Thread.interrupted()) {
                            break;
                        }
                    } else if (wokeAt == -1) {
                        break;
                    }
                }
                wokeAt = System.nanoTime();
                woken[0]++;
                phase = 2 * i + 2;
                if (waiter == Waiter.BLOCK) {
                    LockSupport.unpark(main);
                }
            }
        }, "w77-" + row);
        peer.start();
        for (int i = 0; i < WAKES; i++) {
            wokeAt = 0;
            phase = 2 * i + 1;
            Thread.sleep(5);
            long t0 = System.nanoTime();
            if (kind == Kind.INTERRUPT) {
                peer.interrupt();
            } else {
                wokeAt = -1;
                LockSupport.unpark(peer);
                if (kind == Kind.TWICE) {
                    LockSupport.unpark(peer);
                }
            }
            while (phase != 2 * i + 2) {
                switch (waiter) {
                    case SPIN -> Thread.onSpinWait();
                    case YIELD -> Thread.yield();
                    case BLOCK -> LockSupport.park();
                }
            }
            lat[i] = wokeAt - t0;
        }
        peer.join();
        // A leftover permit from the peer's last unpark must not end a later
        // row's first park early: drop it.
        LockSupport.parkNanos(1);
        Arrays.sort(lat);
        System.out.printf("%-22s %.1f ns/op%n", row + "-p50", (double) lat[WAKES / 2]);
        System.out.printf("%-22s %.1f ns/op%n", row + "-p90", (double) lat[WAKES * 9 / 10]);
        return woken[0];
    }

    public static void main(String[] args) throws Exception {
        System.err.println("processors=" + Runtime.getRuntime().availableProcessors());
        int woken = 0;
        for (Waiter waiter : Waiter.values()) {
            for (Kind kind : Kind.values()) {
                woken += wakeRow(kind, waiter);
            }
        }
        System.out.println("checksum woken=" + woken);
        System.out.println("done");
    }
}
