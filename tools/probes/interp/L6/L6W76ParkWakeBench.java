// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 76, lane L6 -- bench for the wake tail of an
// unpolled park. Page:
// docs/internal/fixed-bugs/interpreter-i73-L6-proposal-untimed-park-without-a-5ms-poll-DONE-20261115.md
// ("Progress (wave 76)").
//
//   CRATONVM_PARK_UNTIMED_NO_POLL=1    a blocking park waits without the 5 ms
//                                      interrupt poll;
//   CRATONVM_PARK_WAKE_AFTER_UNLOCK=1  Unsafe.unpark sets the permit under the
//                                      park state's mutex and wakes after
//                                      releasing it, only when it set the
//                                      permit; Thread.interrupt wakes the
//                                      target's state once (HotSpot's
//                                      Parker::unpark).
//
// A/B, one binary, the four arms interleaved, >= 7 reps, medians of each row;
// --jdk-only and --compatible:
//   cratonvm L6W76ParkWakeBench
//   CRATONVM_PARK_UNTIMED_NO_POLL=1 cratonvm L6W76ParkWakeBench
//   CRATONVM_PARK_WAKE_AFTER_UNLOCK=1 cratonvm L6W76ParkWakeBench
//   CRATONVM_PARK_UNTIMED_NO_POLL=1 CRATONVM_PARK_WAKE_AFTER_UNLOCK=1 cratonvm L6W76ParkWakeBench
//
// Rows (stdout; every row is a percentile of WAKES samples, ns from the wake
// call to the woken peer running):
//   unpark-p50 / -p90          LockSupport.unpark(peer), the peer in its
//                              blocking wait (past the 2 ms running phase);
//   twice-p50 / -p90           two unpark calls back to back (the waker takes
//                              the peer's park-state mutex twice);
//   interrupt-p50 / -p90       peer.interrupt();
//   spin-unpark-p50 / -p90     as unpark, but main busy-waits the 5 ms instead
//                              of Thread.sleep(5): if this row is much faster,
//                              the waker's own way back from its sleep (or the
//                              CPU it wakes on) is in the measured window;
//   churn-unpark-p50 / -p90    as unpark, after CHURN threads were started,
//                              parked untimed and stopped (the shape of
//                              L6W74ParkNoPollBench, whose unpark-wake row is
//                              ~800 us in --compatible where
//                              L6W75UnparkWakeSplit's is ~35 us).
// Stderr per row: late-parks=N/WAKES (rounds whose last park() began after the
// wake call) and park-calls (park() returns beyond the first per round).
// Run with CRATONVM_DBG_PARKLAT=300 to see which part of a slow wake is slow
// ("[parklat] unpark->wake ... woke_by=notify|timeout-tick", "park prologue",
// "park epilogue").
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// checksum unpark=200 twice=200 interrupt=200 spin=200 churn=200 churned=200
// done
// HOTSPOT_EXPECTED_END
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.locks.LockSupport;

public class L6W76ParkWakeBench {
    static final int WAKES = 200;
    static final int CHURN = 200;

    static volatile long wokeAt;
    static volatile long lastParkAt;
    static volatile int phase;
    static volatile boolean stop;

    enum Kind { UNPARK, TWICE, INTERRUPT }

    static void pause(boolean spin) throws InterruptedException {
        if (spin) {
            long until = System.nanoTime() + 5_000_000L;
            while (System.nanoTime() < until) {
                Thread.onSpinWait();
            }
        } else {
            Thread.sleep(5);
        }
    }

    static int wakeRow(String row, Kind kind, boolean spin) throws InterruptedException {
        long[] lat = new long[WAKES];
        int late = 0;
        AtomicInteger woken = new AtomicInteger();
        AtomicInteger extraParks = new AtomicInteger();
        phase = 0;
        Thread peer = new Thread(() -> {
            for (int i = 0; i < WAKES; i++) {
                while (phase != 2 * i + 1) {
                    Thread.onSpinWait();
                }
                int parks = 0;
                while (phase == 2 * i + 1) {
                    lastParkAt = System.nanoTime();
                    LockSupport.park();
                    parks++;
                    if (kind == Kind.INTERRUPT) {
                        if (Thread.interrupted()) {
                            break;
                        }
                    } else if (wokeAt == -1) {
                        break;
                    }
                }
                wokeAt = System.nanoTime();
                extraParks.addAndGet(parks - 1);
                woken.incrementAndGet();
                phase = 2 * i + 2;
            }
        }, "w76-" + row);
        peer.start();
        for (int i = 0; i < WAKES; i++) {
            wokeAt = 0;
            phase = 2 * i + 1;
            pause(spin);
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
                Thread.onSpinWait();
            }
            lat[i] = wokeAt - t0;
            if (lastParkAt > t0) {
                late++;
            }
        }
        peer.join();
        Arrays.sort(lat);
        System.out.printf("%-18s %.1f ns/op%n", row + "-p50", (double) lat[WAKES / 2]);
        System.out.printf("%-18s %.1f ns/op%n", row + "-p90", (double) lat[WAKES * 9 / 10]);
        System.err.println(row + " late-parks=" + late + "/" + WAKES
                + " park-calls=" + extraParks.get());
        return woken.get();
    }

    /** CHURN threads park untimed for 300 ms, then are unparked and joined. */
    static int churn() throws InterruptedException {
        stop = false;
        AtomicInteger parked = new AtomicInteger();
        Thread[] parkers = new Thread[CHURN];
        for (int i = 0; i < CHURN; i++) {
            parkers[i] = new Thread(() -> {
                parked.incrementAndGet();
                while (!stop) {
                    LockSupport.park();
                }
            }, "w76-churn-" + i);
            parkers[i].setDaemon(true);
            parkers[i].start();
        }
        while (parked.get() < CHURN) {
            Thread.sleep(1);
        }
        Thread.sleep(300);
        stop = true;
        for (Thread t : parkers) {
            LockSupport.unpark(t);
        }
        for (Thread t : parkers) {
            t.join();
        }
        return parked.get();
    }

    public static void main(String[] args) throws Exception {
        int unpark = wakeRow("unpark", Kind.UNPARK, false);
        int twice = wakeRow("twice", Kind.TWICE, false);
        int interrupt = wakeRow("interrupt", Kind.INTERRUPT, false);
        int spin = wakeRow("spin-unpark", Kind.UNPARK, true);
        int churned = churn();
        int churn = wakeRow("churn-unpark", Kind.UNPARK, false);
        System.out.println("checksum unpark=" + unpark + " twice=" + twice
                + " interrupt=" + interrupt + " spin=" + spin + " churn=" + churn
                + " churned=" + churned);
        System.out.println("done");
    }
}
